//! Bounded session persistence for Unified's zone document.
//!
//! Block persists the card document it can rebuild widget-for-widget. Unified
//! owns no cards: its history is the terminal's own scrollback, which no
//! process can restore byte-exactly. What it can retain is the bounded record
//! the pane already keeps — command identity, outcome, and the per-zone output
//! snapshot — and replay a readable reconstruction of it above the next
//! prompt, so a restarted pane opens on its own recent work instead of a blank
//! surface.
//!
//! A replayed zone is a reconstruction and is never presented as the original
//! bytes: colour and control sequences are gone (the snapshot is plain text),
//! a truncated snapshot says so, and the whole replay is introduced by one
//! banner line. Record ids are deliberately NOT persisted — a restored zone is
//! issued a fresh id from this process's counter, which keeps the marker
//! injector's monotonic replay defence intact across restarts.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};

use super::{
    CompletedCommandRecord, CompletionProvenance, CompletionProvenanceWire, ZoneOutputSnapshot,
};

/// Zones retained across a restart. The design bound: enough to recognise the
/// session, far short of the 200-zone in-memory cap.
pub(super) const MAX_RESTORED_ZONES: usize = 64;

/// Aggregate ceiling on persisted snapshot text, matching the live per-pane
/// snapshot budget so a restart cannot widen what one pane retains.
pub(super) const MAX_RESTORED_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024;

/// Refuse an oversized file outright rather than decoding it: the writer is
/// bounded, so anything larger was not written by this pane.
pub(super) const MAX_ZONE_HISTORY_FILE_BYTES: u64 = 8 * 1024 * 1024;

const FORMAT_VERSION: u32 = 1;

/// Persistence may describe where a live record originally came from, but
/// replay must never upgrade a weak source. Only an originally trusted or
/// already-recovered record becomes JournalRecovered in this process.
fn replayed_completion_provenance(
    provenance: CompletionProvenanceWire,
    start_mark_seen: bool,
) -> CompletionProvenance {
    match provenance {
        CompletionProvenanceWire::ShellReported if start_mark_seen => {
            CompletionProvenance::JournalRecovered
        }
        CompletionProvenanceWire::ShellReported => CompletionProvenance::ShellReported,
        CompletionProvenanceWire::JournalRecovered => CompletionProvenance::JournalRecovered,
        CompletionProvenanceWire::BoundaryInferred => CompletionProvenance::BoundaryInferred,
        CompletionProvenanceWire::Unknown => CompletionProvenance::Unknown,
    }
}

/// One completed zone as it survives a restart. Mirrors the live record minus
/// its process-local id.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct PersistedZone {
    pub(super) cmd: String,
    #[serde(default)]
    pub(super) exit_code: Option<i32>,
    #[serde(default)]
    pub(super) start_time_ms: Option<u64>,
    #[serde(default)]
    pub(super) end_time_ms: Option<u64>,
    #[serde(default)]
    pub(super) duration_ms: Option<u64>,
    #[serde(default)]
    pub(super) cwd: Option<String>,
    #[serde(default)]
    pub(super) is_background: bool,
    #[serde(default)]
    completion_provenance: CompletionProvenanceWire,
    #[serde(default)]
    start_mark_seen: bool,
    /// Absent when the zone retained no snapshot. An absent snapshot must
    /// never be written as an empty string: the two mean different things to
    /// export, search and the snapshot view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) output: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) output_truncated: bool,
}

#[derive(Debug, serde::Serialize)]
pub(super) struct PersistedZoneSession {
    pub(super) version: u32,
    pub(super) zones: Vec<PersistedZone>,
}

/// Validate every record, but retain only the newest bounded suffix. Do not
/// trust a sequence size hint: even tiny JSON records have substantial inline
/// metadata, and truncating a fully decoded Vec would keep its huge capacity.
struct RestoreZones {
    zones: Vec<PersistedZone>,
    evicted: bool,
}

impl<'de> serde::Deserialize<'de> for RestoreZones {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = RestoreZones;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a sequence of persisted zones")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut zones = VecDeque::with_capacity(MAX_RESTORED_ZONES);
                let mut evicted = false;
                while let Some(zone) = sequence.next_element::<PersistedZone>()? {
                    if zones.len() == MAX_RESTORED_ZONES {
                        zones.pop_front();
                        evicted = true;
                    }
                    zones.push_back(zone);
                }
                Ok(RestoreZones {
                    zones: zones.into(),
                    evicted,
                })
            }
        }
        deserializer.deserialize_seq(Visitor)
    }
}

impl PersistedZone {
    pub(super) fn from_live(
        record: &CompletedCommandRecord,
        snapshot: Option<&ZoneOutputSnapshot>,
    ) -> Self {
        Self {
            cmd: record.cmd.clone(),
            exit_code: record.exit_code,
            start_time_ms: record.start_time_ms,
            end_time_ms: record.end_time_ms,
            duration_ms: record.duration_ms,
            cwd: record.cwd.clone(),
            is_background: record.is_background,
            completion_provenance: record.completion_provenance.into(),
            start_mark_seen: record.start_mark_seen,
            output: snapshot.map(|snapshot| snapshot.plain.clone()),
            output_truncated: snapshot.is_some_and(|snapshot| snapshot.truncated),
        }
    }

    /// The live record for this zone under `id`, which the caller allocates
    /// from this process's counter.
    pub(super) fn into_live(self, id: u64) -> (CompletedCommandRecord, Option<ZoneOutputSnapshot>) {
        let snapshot = self.output.map(|plain| ZoneOutputSnapshot {
            plain,
            truncated: self.output_truncated,
        });
        let completion_provenance = if self.is_background {
            CompletionProvenance::Unknown
        } else {
            replayed_completion_provenance(self.completion_provenance, self.start_mark_seen)
        };
        let start_mark_seen = !self.is_background && self.start_mark_seen;
        let timing_is_authoritative = completion_provenance
            == CompletionProvenance::JournalRecovered
            || (completion_provenance == CompletionProvenance::ShellReported && start_mark_seen);
        (
            CompletedCommandRecord {
                id,
                cmd: if self.is_background {
                    String::new()
                } else {
                    self.cmd
                },
                exit_code: (!self.is_background).then_some(self.exit_code).flatten(),
                start_time_ms: (!self.is_background && timing_is_authoritative)
                    .then_some(self.start_time_ms)
                    .flatten(),
                end_time_ms: (!self.is_background && timing_is_authoritative)
                    .then_some(self.end_time_ms)
                    .flatten(),
                duration_ms: (!self.is_background && timing_is_authoritative)
                    .then_some(self.duration_ms)
                    .flatten(),
                cwd: self.cwd,
                is_background: self.is_background,
                completion_provenance,
                // Zone history predates command-text provenance: fail closed.
                command_source: super::CommandTextSource::Screen,
                start_mark_seen,
            },
            snapshot,
        )
    }

    fn retained_bytes(&self) -> usize {
        self.cmd
            .len()
            .saturating_add(self.cwd.as_ref().map_or(0, String::len))
            .saturating_add(self.output.as_ref().map_or(0, String::len))
    }
}

/// Keep the NEWEST zones that fit both bounds, in chronological order.
///
/// Snapshot text is dropped before a whole zone is: a zone the user can still
/// see the command and outcome of is worth more across a restart than one
/// more zone's output. Zones are always dropped oldest-first so the tail the
/// user just worked in survives.
pub(super) fn bound_persisted_zones(
    mut zones: Vec<PersistedZone>,
    max_zones: usize,
    max_bytes: usize,
) -> Vec<PersistedZone> {
    if zones.len() > max_zones {
        zones.drain(..zones.len() - max_zones);
    }
    let mut total: usize = zones.iter().map(PersistedZone::retained_bytes).sum();
    if total <= max_bytes {
        return zones;
    }
    // Shed output oldest-first; the record itself stays.
    for zone in zones.iter_mut() {
        if total <= max_bytes {
            break;
        }
        if let Some(output) = zone.output.take() {
            total = total.saturating_sub(output.len());
            zone.output_truncated = true;
        }
    }
    while total > max_bytes && !zones.is_empty() {
        let dropped = zones.remove(0);
        total = total.saturating_sub(dropped.retained_bytes());
    }
    zones
}

/// Measure the actual JSON representation without allocating a second copy of
/// snapshots. Escaping can grow plain text by up to six times its byte length.
fn encoded_size(value: &impl serde::Serialize) -> io::Result<usize> {
    #[derive(Default)]
    struct Counter(usize);
    impl io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 = self.0.checked_add(bytes.len()).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "zone history size overflow")
            })?;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter::default();
    serde_json::to_writer(&mut counter, value).map_err(io::Error::other)?;
    Ok(counter.0)
}

/// Serialize a bounded session document that its own reader can reopen.
/// Preserve the newest records, shedding oldest output before command metadata
/// just as the decoded-text budget does. JSON framing and escaping count too.
pub(super) fn encode_session(zones: Vec<PersistedZone>) -> io::Result<Vec<u8>> {
    let mut session = PersistedZoneSession {
        version: FORMAT_VERSION,
        zones: Vec::new(),
    };
    let framing = encoded_size(&session)?;
    session.zones = bound_persisted_zones(zones, MAX_RESTORED_ZONES, MAX_RESTORED_SNAPSHOT_BYTES);
    let mut sizes = session
        .zones
        .iter()
        .map(encoded_size)
        .collect::<io::Result<Vec<_>>>()?;
    let mut total = framing + sizes.iter().sum::<usize>() + sizes.len().saturating_sub(1);
    let limit = MAX_ZONE_HISTORY_FILE_BYTES as usize;
    for (zone, size) in session.zones.iter_mut().zip(&mut sizes) {
        if total <= limit {
            break;
        }
        if let Some(output) = zone.output.take() {
            let was_truncated = zone.output_truncated;
            zone.output_truncated = true;
            let next_size = encoded_size(zone)?;
            if next_size < *size {
                total = total - *size + next_size;
                *size = next_size;
            } else {
                // An empty/short snapshot can cost less than the truncation
                // notice replacing it. Preserve it instead of growing the
                // document while losing its explicit captured-empty meaning.
                zone.output = Some(output);
                zone.output_truncated = was_truncated;
            }
        }
    }
    let mut drop_count = 0;
    while total > limit && drop_count < sizes.len() {
        total -= sizes[drop_count];
        if sizes.len() - drop_count > 1 {
            total -= 1; // the comma separating this record from the next
        }
        drop_count += 1;
    }
    session.zones.drain(..drop_count);
    debug_assert!(total <= limit);
    let mut encoded = Vec::with_capacity(total);
    serde_json::to_writer(&mut encoded, &session).map_err(io::Error::other)?;
    debug_assert_eq!(encoded.len(), total);
    Ok(encoded)
}

/// Decode a session document, rejecting an unknown version outright rather
/// than replaying fields this build cannot interpret.
#[cfg(test)]
fn decode_session(bytes: &[u8]) -> io::Result<Vec<PersistedZone>> {
    decode_session_for_restore(bytes).map(|session| session.zones)
}

struct DecodedSession {
    zones: Vec<PersistedZone>,
    limited: bool,
}

fn decode_session_for_restore(bytes: &[u8]) -> io::Result<DecodedSession> {
    #[derive(serde::Deserialize)]
    struct Version {
        version: u32,
    }
    // Read the version before interpreting records. A future document need
    // not use today's zone shape in order to be identified as unsupported.
    let header: Version = serde_json::from_slice(bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    if header.version != FORMAT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "unsupported zone history version {} (expected {FORMAT_VERSION})",
                header.version
            ),
        ));
    }
    #[derive(serde::Deserialize)]
    struct RestoreSession {
        #[serde(rename = "version")]
        _version: u32,
        zones: RestoreZones,
    }
    let session: RestoreSession = serde_json::from_slice(bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    let RestoreZones { zones, evicted } = session.zones;
    // Compact snapshots only after count eviction. Early byte shedding could
    // lose output that fits once a large old record leaves the retained suffix.
    let limited = evicted
        || zones.iter().fold(0usize, |total, zone| {
            total.saturating_add(zone.retained_bytes())
        }) > MAX_RESTORED_SNAPSHOT_BYTES;
    Ok(DecodedSession {
        zones: bound_persisted_zones(zones, MAX_RESTORED_ZONES, MAX_RESTORED_SNAPSHOT_BYTES),
        limited,
    })
}

/// Read a bounded zone-history file. A file over the ceiling is refused
/// without being decoded; a missing file is simply an empty session.
///
/// The bound is enforced by the shared reader rather than by a `stat` here.
/// A path-based `stat` followed by a separate `read` decided on one file and
/// then read whichever file the path named a moment later, and it believed the
/// size it was told: a file being appended to between the two calls passed the
/// ceiling and then delivered more than it declared. `read_bounded` checks the
/// open descriptor, caps the read itself, and refuses a fifo — which this path
/// would otherwise have blocked the restoring thread on — a device, a hard-
/// linked file, and one another user can write.
#[cfg(test)]
fn read_session(path: &Path) -> io::Result<Vec<PersistedZone>> {
    read_session_for_restore(path).map(|session| session.map_or_else(Vec::new, |s| s.zones))
}

#[cfg(test)]
fn read_session_for_restore(path: &Path) -> io::Result<Option<DecodedSession>> {
    read_session_text(path)?
        .map(|text| decode_session_for_restore(text.as_bytes()))
        .transpose()
}

fn read_session_text(path: &Path) -> io::Result<Option<String>> {
    match jterm_core::snapshot_file::read_bounded(path, MAX_ZONE_HISTORY_FILE_BYTES) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum RestoreStatus {
    #[default]
    Unobserved,
    Missing,
    ValidEmpty,
    Restored,
    RestoredLimited,
    Unsupported,
    Corrupt,
    ReadFailed,
}

// A zone document is one reconstructable session, not a merged command index.
// Without persisted record identities, merging a stale pane would duplicate
// commands or resurrect records deliberately removed from the newer snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionRevision {
    Missing,
    Present {
        device: u64,
        inode: u64,
        len: u64,
        modified: (i64, i64),
        changed: (i64, i64),
    },
}

impl SessionRevision {
    fn observe(path: &Path) -> io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::Missing),
            Err(error) => return Err(error),
        };
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Unified history is not a regular file",
            ));
        }
        Ok(Self::Present {
            device: metadata.dev(),
            inode: metadata.ino(),
            len: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }
}

/// Serialize only bounded raw reads and compare/replace operations. Never keep
/// a lock while decoding, for a pane's lifetime, or wait on the GTK thread. Block history also locks this
/// directory; its atomic writer itself takes no lock, so calling it below is
/// not recursive. A busy sibling save is an ordinary retryable failure.
struct SessionWriteLock(std::fs::File);

impl SessionWriteLock {
    fn acquire(path: &Path) -> io::Result<Self> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
        let directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
            .open(parent)?;
        let metadata = directory.metadata()?;
        if metadata.mode() & 0o022 != 0 && metadata.mode() & nix::libc::S_ISVTX == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Unified history parent is writable by another user or group",
            ));
        }
        // SAFETY: directory owns a live descriptor and flock retains no pointer.
        if unsafe {
            nix::libc::flock(
                directory.as_raw_fd(),
                nix::libc::LOCK_EX | nix::libc::LOCK_NB,
            )
        } != 0
        {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                return Err(io::Error::new(io::ErrorKind::WouldBlock,
                    "Unified history is busy in another save or restore. Existing files are kept; retry after that operation finishes."));
            }
            return Err(error);
        }
        Ok(Self(directory))
    }
}

impl Drop for SessionWriteLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // Explicit unlock keeps an inherited descriptor from extending this
        // critical section after a child is forked. File closes on all paths.
        // SAFETY: the descriptor stays live through this call.
        unsafe {
            nix::libc::flock(self.0.as_raw_fd(), nix::libc::LOCK_UN);
        }
    }
}

fn revision_conflict() -> io::Error {
    io::Error::new(io::ErrorKind::WouldBlock,
        "Unified history changed since this pane restored or saved it. Existing files are kept and this pane's new history is not being saved. Export this pane's work before reopening it.")
}

fn read_observed_session(path: &Path) -> io::Result<(Option<DecodedSession>, SessionRevision)> {
    // The lock also rules out Missing -> Present -> Missing while reading:
    // matching metadata alone cannot prove an absent document stayed absent.
    let lock = SessionWriteLock::acquire(path)?;
    let before = SessionRevision::observe(path)?;
    let text = read_session_text(path)?;
    let after = SessionRevision::observe(path)?;
    drop(lock); // Parsing large bounded documents must not hold the namespace.
    if before != after {
        return Err(revision_conflict());
    }
    let session = text
        .map(|text| decode_session_for_restore(text.as_bytes()))
        .transpose()?;
    Ok((session, after))
}

/// Restore authority belongs to one path and one pane. A missing file is a
/// fresh writable session; a failed read is not an empty session.
#[derive(Debug, Default)]
pub(super) struct SessionPersistence {
    path: Option<PathBuf>,
    status: RestoreStatus,
    revision: std::cell::Cell<Option<SessionRevision>>,
}

impl SessionPersistence {
    pub(super) fn restore(&mut self, path: &Path) -> io::Result<Vec<PersistedZone>> {
        self.path = Some(path.to_path_buf());
        self.revision.set(None);
        match read_observed_session(path) {
            Ok((session, revision)) => {
                self.revision.set(Some(revision));
                self.status = match session.as_ref() {
                    None => RestoreStatus::Missing,
                    Some(session) if session.limited => RestoreStatus::RestoredLimited,
                    Some(session) if session.zones.is_empty() => RestoreStatus::ValidEmpty,
                    Some(_) => RestoreStatus::Restored,
                };
                Ok(session.map_or_else(Vec::new, |session| session.zones))
            }
            Err(error) => {
                self.status = match error.kind() {
                    io::ErrorKind::Unsupported => RestoreStatus::Unsupported,
                    io::ErrorKind::InvalidData => RestoreStatus::Corrupt,
                    _ => RestoreStatus::ReadFailed,
                };
                Err(error)
            }
        }
    }

    /// No retry may turn a failed restore into authority to replace bytes that
    /// were never replayed. Only a real restore on this exact path grants it.
    pub(super) fn save_refusal(&self, path: &Path) -> Option<String> {
        if self.path.as_deref() != Some(path) {
            return Some(
                "Unified history is not being saved because this path has not been restored. \
                 Existing files are kept. Reopen this pane to load its history first."
                    .to_string(),
            );
        }
        if self.status == RestoreStatus::RestoredLimited {
            return Some(
                "Unified history was only partly restored because it exceeds restore limits. \
                 Existing files are kept and new history is not being saved. Keep a backup, \
                 then reduce or move the history file and reopen this pane."
                    .to_string(),
            );
        }
        let reason = match self.status {
            RestoreStatus::Missing | RestoreStatus::ValidEmpty | RestoreStatus::Restored => {
                return None;
            }
            RestoreStatus::Unobserved => "history has not been restored",
            RestoreStatus::RestoredLimited => unreachable!("handled above"),
            RestoreStatus::Unsupported => "its format is not supported by this version",
            RestoreStatus::Corrupt => "the history file is damaged",
            RestoreStatus::ReadFailed => "the history file could not be read",
        };
        Some(format!(
            "Unified history is not being saved because {reason}. Existing files are kept. \
             Repair or move the history file, then reopen this pane."
        ))
    }

    pub(super) fn save(&self, path: &Path, zones: Vec<PersistedZone>) -> io::Result<()> {
        if let Some(message) = self.save_refusal(path) {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, message));
        }
        let _lock = SessionWriteLock::acquire(path)?;
        if self.revision.get() != Some(SessionRevision::observe(path)?) {
            return Err(revision_conflict());
        }
        write_session(path, zones)?;
        self.revision.set(Some(SessionRevision::observe(path)?));
        Ok(())
    }
}

fn write_session(path: &Path, zones: Vec<PersistedZone>) -> io::Result<()> {
    if zones.is_empty() {
        // An empty successfully restored session removes its old document.
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        return Ok(());
    }
    let encoded = encode_session(zones)?;
    super::history::atomic_write(path, |file| {
        use std::io::Write;
        file.write_all(&encoded)
    })
}

/// Terminal bytes that reconstruct one restored zone above the next prompt.
///
/// SGR is emitted only around this module's own framing. The snapshot text is
/// written verbatim as the plain text it is — it carries no escape sequences,
/// having been stripped at capture — so a restored zone cannot re-execute
/// control sequences the original output contained.
pub(super) fn replay_bytes(
    record: &CompletedCommandRecord,
    snapshot: Option<&str>,
    truncated: bool,
) -> Vec<u8> {
    let mut out = Vec::new();
    let cwd = record.cwd.as_deref().unwrap_or("");
    if !cwd.is_empty() {
        out.extend_from_slice(b"\x1b[2m");
        out.extend_from_slice(sanitize_line(cwd).as_bytes());
        out.extend_from_slice(b"\x1b[0m\r\n");
    }
    if !record.cmd.is_empty() {
        out.extend_from_slice(b"\x1b[2m>\x1b[0m ");
        out.extend_from_slice(sanitize_line(&record.cmd).as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    if let Some(snapshot) = snapshot {
        for line in snapshot.split('\n') {
            out.extend_from_slice(sanitize_line(line).as_bytes());
            out.extend_from_slice(b"\r\n");
        }
    }
    if truncated {
        out.extend_from_slice(b"\x1b[2m");
        out.extend_from_slice(b"[output truncated]");
        out.extend_from_slice(b"\x1b[0m\r\n");
    }
    out
}

/// One dim line introducing the replay, so restored rows are never mistaken
/// for output this session produced.
pub(super) fn replay_banner(zone_count: usize) -> Vec<u8> {
    format!("\x1b[2m-- restored session: {zone_count} recent commands --\x1b[0m\r\n").into_bytes()
}

/// Drop every C0/C1 control byte from a persisted field before it reaches the
/// terminal. Persisted text is data, not a program: a record written by an
/// older build, a shared file, or a hand-edited one must not be able to move
/// the cursor, open a hyperlink, or start an escape sequence during replay.
fn sanitize_line(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control())
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone(cmd: &str, output: Option<&str>) -> PersistedZone {
        PersistedZone {
            cmd: cmd.to_string(),
            exit_code: Some(0),
            start_time_ms: None,
            end_time_ms: None,
            duration_ms: Some(5),
            cwd: Some("/tmp".to_string()),
            is_background: false,
            completion_provenance: CompletionProvenanceWire::ShellReported,
            start_mark_seen: true,
            output: output.map(str::to_string),
            output_truncated: false,
        }
    }

    #[test]
    fn decoding_preserves_required_fields_and_version_precedence() {
        for document in [
            r#"{"zones":[]}"#,
            r#"{"version":1}"#,
            r#"{"version":1,"version":1,"zones":[]}"#,
            r#"{"version":1,"zones":[],"zones":[]}"#,
            r#"{"version":1,"zones":[{}]}"#,
            r#"{"version":1,"zones":[{"cmd":"a","cmd":"b"}]}"#,
        ] {
            assert_eq!(
                decode_session_for_restore(document.as_bytes())
                    .err()
                    .unwrap()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
        for document in [
            r#"{"version":2,"zones":false}"#,
            r#"{"version":2,"future_payload":{}}"#,
        ] {
            assert_eq!(
                decode_session_for_restore(document.as_bytes())
                    .err()
                    .unwrap()
                    .kind(),
                io::ErrorKind::Unsupported
            );
        }
        let empty =
            decode_session_for_restore(br#"{"extra":true,"zones":[],"version":1}"#).unwrap();
        assert!(empty.zones.is_empty());
        assert!(!empty.limited);
    }

    #[test]
    fn decoding_ignores_untrusted_sequence_size_hints() {
        struct HintedSequence(usize);
        impl<'de> serde::de::SeqAccess<'de> for HintedSequence {
            type Error = serde_json::Error;

            fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>, Self::Error>
            where
                T: serde::de::DeserializeSeed<'de>,
            {
                if self.0 == 0 {
                    return Ok(None);
                }
                self.0 -= 1;
                seed.deserialize(serde_json::json!({ "cmd": "small" }))
                    .map(Some)
            }

            fn size_hint(&self) -> Option<usize> {
                Some(usize::MAX)
            }
        }
        let decoded: RestoreZones = serde::Deserialize::deserialize(
            serde::de::value::SeqAccessDeserializer::new(HintedSequence(2)),
        )
        .unwrap();
        assert!(!decoded.evicted);
        assert_eq!(decoded.zones.len(), 2);
        assert!(decoded.zones.capacity() <= MAX_RESTORED_ZONES);
    }

    #[test]
    fn decoding_many_small_zones_retains_only_bounded_record_capacity() {
        let count = 500_000;
        let mut document = String::from(r#"{"version":1,"zones":["#);
        for index in 0..count {
            if index != 0 {
                document.push(',');
            }
            document.push_str(r#"{"cmd":""}"#);
        }
        document.push_str("]}");
        assert!(document.len() < MAX_ZONE_HISTORY_FILE_BYTES as usize);
        let decoded = decode_session_for_restore(document.as_bytes()).unwrap();
        assert!(decoded.limited);
        assert_eq!(decoded.zones.len(), MAX_RESTORED_ZONES);
        eprintln!(
            "input_bytes={} records={} retained_capacity={} inline_allocation_bytes={}",
            document.len(),
            count,
            decoded.zones.capacity(),
            decoded.zones.capacity() * std::mem::size_of::<PersistedZone>()
        );
        assert!(decoded.zones.capacity() <= MAX_RESTORED_ZONES);
    }

    #[test]
    fn decoding_count_eviction_precedes_snapshot_byte_compaction() {
        let mut zones = vec![zone(&"x".repeat(3 * 1024 * 1024), None)];
        let output = "y".repeat(32 * 1024);
        for index in 0..MAX_RESTORED_ZONES {
            zones.push(zone(&format!("newest-{index}"), Some(&output)));
        }
        let document = serde_json::to_vec(&PersistedZoneSession {
            version: FORMAT_VERSION,
            zones: zones.clone(),
        })
        .unwrap();
        assert!(document.len() < MAX_ZONE_HISTORY_FILE_BYTES as usize);
        let decoded = decode_session_for_restore(&document).unwrap();
        assert!(decoded.limited);
        assert_eq!(decoded.zones, zones[1..]);
        assert!(decoded.zones.iter().all(|zone| !zone.output_truncated));
    }

    #[test]
    fn decoding_validates_evicted_records_and_the_document_tail() {
        let good = r#"{"cmd":"valid"},"#.repeat(MAX_RESTORED_ZONES + 1);
        for document in [
            format!(r#"{{"version":1,"zones":[{{"cmd":false}},{good}{{"cmd":"last"}}]}}"#),
            format!(r#"{{"version":1,"zones":[{good}{{"cmd":false}}]}}"#),
            format!(r#"{{"version":1,"zones":[{good}{{"cmd":"last"}}]}} trailing"#),
        ] {
            assert_eq!(
                decode_session_for_restore(document.as_bytes())
                    .err()
                    .unwrap()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn bounds_keep_the_newest_zones_and_shed_output_before_records() {
        let zones = vec![
            zone("first", Some(&"a".repeat(64))),
            zone("second", Some(&"b".repeat(64))),
            zone("third", Some(&"c".repeat(64))),
        ];
        let bounded = bound_persisted_zones(zones.clone(), 2, usize::MAX);
        assert_eq!(
            bounded.iter().map(|z| z.cmd.as_str()).collect::<Vec<_>>(),
            vec!["second", "third"],
            "the oldest zone is dropped first"
        );

        // A budget that fits the three commands plus exactly one output: the
        // two older zones survive without theirs rather than being dropped.
        let budget = "first".len() + "second".len() + "third".len() + 3 * "/tmp".len() + 64;
        let bounded = bound_persisted_zones(zones, 3, budget);
        assert_eq!(bounded.len(), 3, "records outlive their output");
        assert_eq!(bounded[0].output, None);
        assert!(bounded[0].output_truncated, "shedding output is truncation");
        assert_eq!(bounded[1].output, None);
        assert_eq!(
            bounded[2].output.as_deref(),
            Some("c".repeat(64).as_str()),
            "the newest zone keeps the output the budget can still afford"
        );
    }

    #[test]
    fn an_unfittable_zone_set_drops_records_oldest_first() {
        let zones = vec![zone("aaaa", None), zone("bbbb", None)];
        let bounded = bound_persisted_zones(zones, 8, 8);
        assert_eq!(
            bounded.iter().map(|z| z.cmd.as_str()).collect::<Vec<_>>(),
            vec!["bbbb"]
        );
    }

    #[test]
    fn replay_budget_counts_working_directory_metadata() {
        let mut older = zone("first", None);
        older.cwd = Some("x".repeat(256));
        let mut newer = zone("last", None);
        newer.cwd = None;
        let bounded = bound_persisted_zones(vec![older, newer], 64, 128);
        assert_eq!(
            bounded.len(),
            1,
            "cwd metadata must share the retained byte budget"
        );
        assert_eq!(bounded[0].cmd, "last");
    }

    #[test]
    fn an_encoded_session_always_fits_its_own_reader_limit() {
        // A real plain-text snapshot can consist of quotes/backslashes. JSON
        // doubles those bytes even though the retained text fits its budget.
        let zones = (0..MAX_RESTORED_ZONES)
            .map(|_| {
                let mut record = zone(&"\\".repeat(32 * 1024), Some(&"\\".repeat(64 * 1024)));
                record.cwd = None;
                record
            })
            .collect();
        let bounded = bound_persisted_zones(zones, MAX_RESTORED_ZONES, MAX_RESTORED_SNAPSHOT_BYTES);
        let encoded = encode_session(bounded).expect("bounded session encodes");
        assert!(
            encoded.len() as u64 <= MAX_ZONE_HISTORY_FILE_BYTES,
            "writer emitted {} bytes, but its reader accepts only {}",
            encoded.len(),
            MAX_ZONE_HISTORY_FILE_BYTES
        );
        let decoded = decode_session(&encoded).expect("our own document must restore");
        assert_eq!(decoded.len(), MAX_RESTORED_ZONES);
        assert_eq!(decoded.last().unwrap().cmd.len(), 32 * 1024);
    }

    #[test]
    fn json_budget_sheds_records_after_empty_output_and_sixfold_escaping() {
        let zones = (0..MAX_RESTORED_ZONES)
            .map(|index| {
                let mut record = zone(
                    &format!("{}record-{index}", "\0".repeat(64 * 1024 - 10)),
                    Some(""),
                );
                record.cwd = None;
                record
            })
            .collect();
        let encoded = encode_session(zones).expect("control-heavy metadata encodes");
        assert!(encoded.len() as u64 <= MAX_ZONE_HISTORY_FILE_BYTES);
        let decoded = decode_session(&encoded).expect("bounded escaped metadata restores");
        assert!(!decoded.is_empty());
        assert!(decoded.len() < MAX_RESTORED_ZONES);
        assert!(decoded.last().unwrap().cmd.ends_with("record-63"));
        assert!(decoded
            .iter()
            .all(|zone| zone.output.as_deref() == Some("") && !zone.output_truncated));
    }

    #[test]
    fn encoded_size_counts_unicode_controls_and_json_framing_exactly() {
        for output in [None, Some(""), Some("漢字🦀\n\t\r\0\u{001f}\\\"")] {
            let record = zone("command 漢字🦀\0", output);
            assert_eq!(
                encoded_size(&record).unwrap(),
                serde_json::to_vec(&record).unwrap().len()
            );
        }
    }

    #[test]
    fn round_trip_preserves_identity_outcome_and_snapshot_absence() {
        let zones = vec![zone("with", Some("output")), zone("without", None)];
        let encoded = encode_session(zones.clone()).expect("encodes");
        let decoded = decode_session(&encoded).expect("decodes");
        assert_eq!(decoded, zones);

        let text = String::from_utf8(encoded).expect("utf-8");
        assert!(
            !text.contains("\"output\":null"),
            "an absent snapshot is omitted, never written as a null or empty stand-in"
        );
    }

    #[test]
    fn legacy_v1_defaults_to_unknown_incomplete_instead_of_gaining_trust() {
        let legacy = br#"{"version":1,"zones":[{"cmd":"legacy","exit_code":0}]}"#;
        let mut decoded = decode_session(legacy).expect("legacy v1 decodes");
        let (record, _) = decoded.remove(0).into_live(9);
        assert_eq!(
            record.completion_provenance,
            super::super::CompletionProvenance::Unknown
        );
        assert!(!record.start_mark_seen);
        assert_eq!(
            record.lifecycle_health(),
            super::super::BlockLifecycleHealth::Incomplete
        );
    }

    #[test]
    fn inferred_completion_round_trip_is_never_upgraded_to_recovered() {
        let mut inferred = zone("inferred", None);
        inferred.completion_provenance = CompletionProvenanceWire::BoundaryInferred;
        inferred.start_mark_seen = true;
        let encoded = encode_session(vec![inferred]).unwrap();
        let mut decoded = decode_session(&encoded).unwrap();
        let (record, _) = decoded.remove(0).into_live(10);
        assert_eq!(
            record.completion_provenance,
            super::super::CompletionProvenance::BoundaryInferred
        );
        assert_eq!(
            record.lifecycle_health(),
            super::super::BlockLifecycleHealth::Degraded
        );
        assert_eq!(record.start_time_ms, None);
        assert_eq!(record.end_time_ms, None);
        assert_eq!(record.duration_ms, None);
    }

    #[test]
    fn contradictory_shell_report_without_start_mark_stays_degraded() {
        let mut contradictory = zone("contradictory", None);
        contradictory.completion_provenance = CompletionProvenanceWire::ShellReported;
        contradictory.start_mark_seen = false;
        let encoded = encode_session(vec![contradictory]).unwrap();
        let mut decoded = decode_session(&encoded).unwrap();
        let (record, _) = decoded.remove(0).into_live(11);
        assert_eq!(
            record.completion_provenance,
            super::super::CompletionProvenance::ShellReported
        );
        assert_eq!(
            record.lifecycle_health(),
            super::super::BlockLifecycleHealth::Degraded
        );
        assert_eq!(record.start_time_ms, None);
        assert_eq!(record.end_time_ms, None);
        assert_eq!(record.duration_ms, None);
    }

    #[test]
    fn contradictory_background_fields_are_normalized_to_background_semantics() {
        let mut background = zone("must-not-be-a-command", None);
        background.is_background = true;
        background.exit_code = Some(9);
        background.start_time_ms = Some(1);
        background.end_time_ms = Some(2);
        background.duration_ms = Some(1);
        background.completion_provenance = CompletionProvenanceWire::ShellReported;
        background.start_mark_seen = true;
        let (record, _) = background.into_live(12);
        assert!(record.is_background);
        assert_eq!(record.cmd, "");
        assert_eq!(record.exit_code, None);
        assert_eq!(record.start_time_ms, None);
        assert_eq!(record.end_time_ms, None);
        assert_eq!(record.duration_ms, None);
        assert_eq!(
            record.completion_provenance,
            super::super::CompletionProvenance::Unknown
        );
        assert!(!record.start_mark_seen);
    }

    #[test]
    fn an_unknown_version_is_refused_rather_than_partially_replayed() {
        let document = br#"{"version":9999,"zones":[{"cmd":"x"}]}"#;
        let error = decode_session(document).expect_err("refuses");
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn replayed_fields_cannot_execute_control_sequences() {
        let (record, snapshot) =
            zone("ls\x1b]8;;block://deadbeef/7\x1b\\", Some("out\x1b[2Jline")).into_live(3);
        let truncated = snapshot.as_ref().is_some_and(|s| s.truncated);
        let bytes = replay_bytes(
            &record,
            snapshot.as_ref().map(|s| s.plain.as_str()),
            truncated,
        );
        let rendered = String::from_utf8(bytes).expect("utf-8");
        // Persisted text reaches the surface as inert characters. Without an
        // introducer it cannot open a hyperlink, erase the screen, or move the
        // cursor, so a marker-shaped substring left in it is display text and
        // not authority — chrome only trusts a URI VTE reports as a link.
        let framing_escapes = rendered.matches('\x1b').count();
        assert_eq!(
            framing_escapes,
            rendered.matches("\x1b[2m").count() + rendered.matches("\x1b[0m").count(),
            "the only escapes in a replay are this module's own dim framing"
        );
        assert!(!rendered.contains("\x1b]8"), "no OSC 8 from persisted text");
        assert!(
            !rendered.contains("\x1b[2J"),
            "no erase from persisted text"
        );
        assert!(
            rendered.contains("ls]8;;block://deadbeef/7"),
            "the command text survives, minus its control bytes: {rendered:?}"
        );
        assert!(rendered.contains("out[2Jline"), "output survives inert");
    }

    #[test]
    fn a_truncated_snapshot_says_so_in_the_replay() {
        let (record, _) = zone("build", None).into_live(1);
        let bytes = replay_bytes(&record, Some("tail"), true);
        let rendered = String::from_utf8(bytes).expect("utf-8");
        assert!(rendered.contains("[output truncated]"));

        let bytes = replay_bytes(&record, Some("tail"), false);
        let rendered = String::from_utf8(bytes).expect("utf-8");
        assert!(!rendered.contains("[output truncated]"));
    }

    /// A file this pane never wrote must not be decoded on the strength of
    /// being parseable: an absent one is an empty session, an oversized or
    /// non-regular one is refused.
    #[test]
    fn reading_a_session_refuses_what_the_writer_could_not_have_produced() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "forge-zone-history-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");

        let missing = dir.join("absent.json");
        assert!(read_session(&missing).expect("absent is empty").is_empty());

        // The real writer creates this document 0600; a fixture written with
        // the ambient umask would be group-writable under the common
        // `umask 002` and correctly refused by the hardened reader.
        let write_fixture = |path: &Path, bytes: &[u8]| {
            use std::os::unix::fs::PermissionsExt;

            std::fs::write(path, bytes).expect("write");
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .expect("private fixture");
        };

        let file = dir.join("zones.json");
        let encoded = encode_session(vec![zone("echo hi", Some("hi"))]).expect("encodes");
        write_fixture(&file, &encoded);
        let restored = read_session(&file).expect("reads");
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].output.as_deref(), Some("hi"));

        write_fixture(&file, &vec![b'x'; MAX_ZONE_HISTORY_FILE_BYTES as usize + 1]);
        let error = read_session(&file).expect_err("refuses an oversized file");
        // Deliberately re-pinned: the shared reader reports an oversize
        // document as `FileTooLarge`, where the local stat-then-read reported
        // `InvalidData`. Same refusal, a more accurate kind.
        assert_eq!(error.kind(), io::ErrorKind::FileTooLarge);

        let error = read_session(&dir).expect_err("refuses a directory");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        // Hazards the former stat-then-read could not see.
        let target = dir.join("target.json");
        write_fixture(&target, &encoded);
        let linked = dir.join("symlinked.json");
        std::os::unix::fs::symlink(&target, &linked).expect("symlink");
        assert!(
            read_session(&linked).is_err(),
            "a history path replaced by a symlink is not this pane's document"
        );

        let group_writable = dir.join("group-writable.json");
        write_fixture(&group_writable, &encoded);
        {
            use std::os::unix::fs::PermissionsExt;

            std::fs::set_permissions(&group_writable, std::fs::Permissions::from_mode(0o620))
                .expect("relax");
        }
        assert!(
            read_session(&group_writable).is_err(),
            "another user could have written every command replayed from this"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    struct SessionFixture {
        directory: PathBuf,
        path: PathBuf,
    }

    impl SessionFixture {
        fn new() -> Self {
            use std::os::unix::fs::PermissionsExt;
            let directory =
                std::env::temp_dir().join(format!("anvil-zone-recovery-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&directory).unwrap();
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
            let path = directory.join("zones.json");
            Self { directory, path }
        }

        fn write(&self, bytes: &[u8]) {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(&self.path, bytes).unwrap();
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    impl Drop for SessionFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    #[test]
    fn failed_restore_preserves_exact_bytes_on_empty_close_and_later_save() {
        for original in [
            &b"{\"version\":2,\"future_document\":{\"kept\":true}}"[..],
            &b"{\"version\":1,\"zones\":["[..],
            &b""[..],
            &[0xff, 0xfe][..],
        ] {
            for later_zones in [Vec::new(), vec![zone("new command", Some("new output"))]] {
                let fixture = SessionFixture::new();
                fixture.write(original);
                let mut state = SessionPersistence::default();
                assert!(state.restore(&fixture.path).is_err());
                assert!(state.save(&fixture.path, later_zones).is_err());
                assert_eq!(std::fs::read(&fixture.path).unwrap(), original);
            }
        }
    }

    #[test]
    fn failed_restore_does_not_gain_save_authority_when_disk_is_repaired() {
        let fixture = SessionFixture::new();
        fixture.write(b"not json");
        let mut state = SessionPersistence::default();
        assert!(state.restore(&fixture.path).is_err());
        let repaired = encode_session(vec![zone("recovered", Some("preserved output"))]).unwrap();
        fixture.write(&repaired);
        for _ in 0..2 {
            assert!(state.save(&fixture.path, vec![zone("live", None)]).is_err());
            assert_eq!(std::fs::read(&fixture.path).unwrap(), repaired);
        }
        let mut reopened = SessionPersistence::default();
        let restored = reopened.restore(&fixture.path).unwrap();
        assert_eq!(restored[0].cmd, "recovered");
        reopened.save(&fixture.path, restored).unwrap();
        assert_eq!(read_session(&fixture.path).unwrap()[0].cmd, "recovered");
    }

    #[test]
    fn restore_outcomes_distinguish_fresh_empty_unsupported_corrupt_and_read_failure() {
        let fixture = SessionFixture::new();
        let mut state = SessionPersistence::default();
        assert_eq!(state.status, RestoreStatus::Unobserved);
        assert!(state.save(&fixture.path, Vec::new()).is_err());
        assert!(!fixture.path.exists());

        assert!(state.restore(&fixture.path).unwrap().is_empty());
        assert_eq!(state.status, RestoreStatus::Missing);
        state
            .save(&fixture.path, vec![zone("fresh", None)])
            .unwrap();
        assert_eq!(read_session(&fixture.path).unwrap()[0].cmd, "fresh");

        let restored = state.restore(&fixture.path).unwrap();
        assert_eq!(state.status, RestoreStatus::Restored);
        state.save(&fixture.path, restored).unwrap();

        fixture.write(br#"{"version":1,"zones":[]}"#);
        assert!(state.restore(&fixture.path).unwrap().is_empty());
        assert_eq!(state.status, RestoreStatus::ValidEmpty);
        state.save(&fixture.path, Vec::new()).unwrap();
        assert!(!fixture.path.exists());

        fixture.write(br#"{"version":2,"future_payload":{}}"#);
        assert!(state.restore(&fixture.path).is_err());
        assert_eq!(state.status, RestoreStatus::Unsupported);

        fixture.write(br#"{"version":1,"zones":false}"#);
        assert!(state.restore(&fixture.path).is_err());
        assert_eq!(state.status, RestoreStatus::Corrupt);

        assert!(state.restore(&fixture.directory).is_err());
        assert_eq!(state.status, RestoreStatus::ReadFailed);
        assert!(state.save(&fixture.directory, Vec::new()).is_err());
        assert!(fixture.path.is_file());
    }

    #[test]
    fn write_authority_cannot_follow_a_changed_history_path() {
        let original = SessionFixture::new();
        let changed = SessionFixture::new();
        let preserved = br#"{"version":9,"future_payload":true}"#;
        changed.write(preserved);
        let mut state = SessionPersistence::default();
        state.restore(&original.path).unwrap();
        for zones in [Vec::new(), vec![zone("new", None)]] {
            assert!(state.save(&changed.path, zones).is_err());
            assert_eq!(std::fs::read(&changed.path).unwrap(), preserved);
        }
        assert!(!original.path.exists());
    }

    #[test]
    fn recovery_refusals_explain_preservation_without_disclosing_file_contents() {
        let fixture = SessionFixture::new();
        fixture.write(b"private broken content");
        let mut state = SessionPersistence::default();
        assert!(state.restore(&fixture.path).is_err());
        let message = state.save_refusal(&fixture.path).unwrap();
        assert!(message.contains("not being saved"));
        assert!(message.contains("Existing files are kept"));
        assert!(message.contains("reopen this pane"));
        assert!(!message.contains("private broken content"));
    }

    #[test]
    fn limited_restore_never_becomes_authority_to_delete_unreplayed_data() {
        let oversized = zone(&"c".repeat(MAX_RESTORED_SNAPSHOT_BYTES + 1), None);
        let output_heavy = zone("output", Some(&"o".repeat(MAX_RESTORED_SNAPSHOT_BYTES + 1)));
        let many = (0..=MAX_RESTORED_ZONES)
            .map(|_| zone("small", None))
            .collect();
        for zones in [vec![oversized], vec![output_heavy], many] {
            let fixture = SessionFixture::new();
            // Model an older producer: serialize directly so today's writer
            // does not repair this fixture before the restore boundary sees it.
            let original = serde_json::to_vec(&PersistedZoneSession {
                version: FORMAT_VERSION,
                zones,
            })
            .unwrap();
            assert!(original.len() < MAX_ZONE_HISTORY_FILE_BYTES as usize);
            fixture.write(&original);
            let mut state = SessionPersistence::default();
            let restored = state.restore(&fixture.path).unwrap();
            assert_eq!(state.status, RestoreStatus::RestoredLimited);
            assert!(state.save(&fixture.path, restored).is_err());
            assert_eq!(std::fs::read(&fixture.path).unwrap(), original);
            assert!(state.save(&fixture.path, Vec::new()).is_err());
            assert_eq!(std::fs::read(&fixture.path).unwrap(), original);
        }
    }

    #[test]
    fn restored_zones_take_fresh_ids_so_marker_authority_stays_monotonic() {
        let (record, snapshot) = zone("echo hi", Some("hi")).into_live(41);
        assert_eq!(record.id, 41, "the caller's id wins, not a persisted one");
        assert_eq!(snapshot.expect("snapshot").plain, "hi");
    }
    #[test]
    fn concurrent_panes_cannot_overwrite_newer_saved_records() {
        let fixture = SessionFixture::new();
        let mut first = SessionPersistence::default();
        let mut second = SessionPersistence::default();
        assert!(first.restore(&fixture.path).unwrap().is_empty());
        assert!(second.restore(&fixture.path).unwrap().is_empty());
        first
            .save(
                &fixture.path,
                vec![zone("pane A new command", Some("A output"))],
            )
            .unwrap();
        let saved = std::fs::read(&fixture.path).unwrap();
        let result = second.save(
            &fixture.path,
            vec![zone("pane B new command", Some("B output"))],
        );
        let mut reopened = SessionPersistence::default();
        let restored = reopened.restore(&fixture.path).unwrap();
        eprintln!(
            "new-record overlap: save={result:?}, reopened={:?}",
            restored.iter().map(|z| z.cmd.as_str()).collect::<Vec<_>>()
        );
        assert!(
            result.is_err(),
            "a stale pane must not replace another pane's new records"
        );
        assert_eq!(std::fs::read(&fixture.path).unwrap(), saved);
        assert_eq!(restored[0].cmd, "pane A new command");
    }

    #[test]
    fn concurrent_panes_cannot_resurrect_removed_records() {
        let mut conflicts = Vec::new();
        for clear_all in [false, true] {
            let fixture = SessionFixture::new();
            fixture.write(
                &encode_session(vec![
                    zone("keep", Some("kept")),
                    zone("deleted", Some("gone")),
                ])
                .unwrap(),
            );
            let mut first = SessionPersistence::default();
            let mut stale = SessionPersistence::default();
            first.restore(&fixture.path).unwrap();
            let stale_zones = stale.restore(&fixture.path).unwrap();
            first
                .save(
                    &fixture.path,
                    if clear_all {
                        vec![]
                    } else {
                        vec![zone("keep", Some("kept"))]
                    },
                )
                .unwrap();
            let saved = std::fs::read(&fixture.path).ok();
            let result = stale.save(&fixture.path, stale_zones);
            let mut reopened = SessionPersistence::default();
            let restored = reopened.restore(&fixture.path).unwrap();
            eprintln!(
                "delete overlap (clear={clear_all}): save={result:?}, reopened={:?}",
                restored.iter().map(|z| z.cmd.as_str()).collect::<Vec<_>>()
            );
            if result.is_ok()
                || std::fs::read(&fixture.path).ok() != saved
                || restored.iter().any(|z| z.cmd == "deleted")
            {
                conflicts.push(clear_all);
            }
        }
        assert!(
            conflicts.is_empty(),
            "stale close resurrected history for clear={conflicts:?}"
        );
    }

    #[test]
    fn concurrent_empty_close_cannot_remove_newer_records() {
        let fixture = SessionFixture::new();
        let mut active = SessionPersistence::default();
        let mut empty = SessionPersistence::default();
        active.restore(&fixture.path).unwrap();
        empty.restore(&fixture.path).unwrap();
        active
            .save(
                &fixture.path,
                vec![zone("active work", Some("valuable output"))],
            )
            .unwrap();
        let saved = std::fs::read(&fixture.path).unwrap();
        let result = empty.save(&fixture.path, Vec::new());
        let mut reopened = SessionPersistence::default();
        let restored = reopened.restore(&fixture.path).unwrap();
        eprintln!(
            "empty-close overlap: save={result:?}, reopened={:?}",
            restored.iter().map(|z| z.cmd.as_str()).collect::<Vec<_>>()
        );
        assert!(
            result.is_err(),
            "an empty stale pane must not remove another pane's saved work"
        );
        assert_eq!(std::fs::read(&fixture.path).unwrap(), saved);
        assert_eq!(restored[0].cmd, "active work");
    }

    #[test]
    fn own_successful_saves_refresh_revision_and_keep_clear_authoritative() {
        let fixture = SessionFixture::new();
        let mut state = SessionPersistence::default();
        state.restore(&fixture.path).unwrap();
        for cmd in ["first", "second", "third"] {
            state
                .save(&fixture.path, vec![zone(cmd, Some(cmd))])
                .unwrap();
            assert_eq!(read_session(&fixture.path).unwrap()[0].cmd, cmd);
        }
        state.save(&fixture.path, Vec::new()).unwrap();
        assert!(!fixture.path.exists());
        state.save(&fixture.path, Vec::new()).unwrap();
        state
            .save(&fixture.path, vec![zone("after clear", None)])
            .unwrap();
        let mut reopened = SessionPersistence::default();
        assert_eq!(
            reopened.restore(&fixture.path).unwrap()[0].cmd,
            "after clear"
        );
    }

    #[test]
    fn externally_replaced_or_removed_history_is_preserved_until_reopen() {
        for replacement in [Some(vec![zone("external replacement", Some("kept"))]), None] {
            let fixture = SessionFixture::new();
            fixture.write(&encode_session(vec![zone("original", None)]).unwrap());
            let mut state = SessionPersistence::default();
            state.restore(&fixture.path).unwrap();
            match replacement {
                Some(zones) => write_session(&fixture.path, zones).unwrap(),
                None => std::fs::remove_file(&fixture.path).unwrap(),
            }
            let bytes = std::fs::read(&fixture.path).ok();
            for _ in 0..2 {
                let result = state.save(&fixture.path, vec![zone("stale", None)]);
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::WouldBlock);
                assert_eq!(std::fs::read(&fixture.path).ok(), bytes);
            }
        }
    }

    #[test]
    fn writer_lock_is_nonblocking_and_released_for_retry() {
        use std::os::fd::AsRawFd;
        let fixture = SessionFixture::new();
        let mut state = SessionPersistence::default();
        state.restore(&fixture.path).unwrap();
        let directory = std::fs::File::open(&fixture.directory).unwrap();
        // SAFETY: this fixture owns the live directory descriptor.
        assert_eq!(
            unsafe {
                nix::libc::flock(
                    directory.as_raw_fd(),
                    nix::libc::LOCK_EX | nix::libc::LOCK_NB,
                )
            },
            0
        );
        let refused = state.save(&fixture.path, vec![zone("retry me", None)]);
        // Release before assertions so a failing test never strands the lock.
        // SAFETY: this fixture still owns the directory descriptor.
        assert_eq!(
            unsafe { nix::libc::flock(directory.as_raw_fd(), nix::libc::LOCK_UN) },
            0
        );
        assert_eq!(refused.unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert!(!fixture.path.exists());
        state
            .save(&fixture.path, vec![zone("retry me", None)])
            .unwrap();
        assert_eq!(read_session(&fixture.path).unwrap()[0].cmd, "retry me");
        // A successful write's guard must be gone too.
        assert_eq!(
            unsafe {
                nix::libc::flock(
                    directory.as_raw_fd(),
                    nix::libc::LOCK_EX | nix::libc::LOCK_NB,
                )
            },
            0
        );
        assert_eq!(
            unsafe { nix::libc::flock(directory.as_raw_fd(), nix::libc::LOCK_UN) },
            0
        );
    }

    #[test]
    fn simultaneous_writers_have_one_successful_snapshot() {
        use std::sync::{Arc, Barrier};
        for _ in 0..8 {
            let fixture = SessionFixture::new();
            let mut first = SessionPersistence::default();
            let mut second = SessionPersistence::default();
            first.restore(&fixture.path).unwrap();
            second.restore(&fixture.path).unwrap();
            let start = Arc::new(Barrier::new(2));
            let handles: Vec<_> = [(first, "one"), (second, "two")]
                .into_iter()
                .map(|(state, cmd)| {
                    let path = fixture.path.clone();
                    let start = Arc::clone(&start);
                    std::thread::spawn(move || {
                        start.wait();
                        let result = state.save(&path, vec![zone(cmd, Some(cmd))]);
                        (cmd, result)
                    })
                })
                .collect();
            let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
            assert_eq!(
                results.iter().filter(|(_, r)| r.is_ok()).count(),
                1,
                "{results:?}"
            );
            let winner = results.iter().find(|(_, r)| r.is_ok()).unwrap().0;
            assert_eq!(read_session(&fixture.path).unwrap()[0].cmd, winner);
        }
    }

    #[test]
    fn failed_reread_never_keeps_previous_write_authority() {
        let fixture = SessionFixture::new();
        let mut state = SessionPersistence::default();
        state.restore(&fixture.path).unwrap();
        state
            .save(&fixture.path, vec![zone("original", None)])
            .unwrap();
        fixture.write(b"unreadable JSON");
        assert!(state.restore(&fixture.path).is_err());
        fixture.write(&encode_session(vec![zone("repaired", None)]).unwrap());
        let repaired = std::fs::read(&fixture.path).unwrap();
        assert!(state.save(&fixture.path, vec![zone("new", None)]).is_err());
        assert_eq!(std::fs::read(&fixture.path).unwrap(), repaired);
    }
    #[test]
    fn restore_lock_conflict_never_grants_snapshot_authority() {
        use std::os::fd::AsRawFd;
        let fixture = SessionFixture::new();
        fixture.write(&encode_session(vec![zone("saved", None)]).unwrap());
        let directory = std::fs::File::open(&fixture.directory).unwrap();
        // SAFETY: this fixture owns the live directory descriptor.
        assert_eq!(
            unsafe {
                nix::libc::flock(
                    directory.as_raw_fd(),
                    nix::libc::LOCK_EX | nix::libc::LOCK_NB,
                )
            },
            0
        );
        let mut state = SessionPersistence::default();
        let observed = state.restore(&fixture.path);
        // SAFETY: the fixture still owns this descriptor.
        assert_eq!(
            unsafe { nix::libc::flock(directory.as_raw_fd(), nix::libc::LOCK_UN) },
            0
        );
        assert_eq!(observed.unwrap_err().kind(), io::ErrorKind::WouldBlock);
        let original = std::fs::read(&fixture.path).unwrap();
        assert!(state.save(&fixture.path, Vec::new()).is_err());
        assert_eq!(std::fs::read(&fixture.path).unwrap(), original);
        let zones = state.restore(&fixture.path).unwrap();
        state.save(&fixture.path, zones).unwrap();
    }
}
