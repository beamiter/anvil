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

use std::io;
use std::path::Path;

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

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(super) struct PersistedZoneSession {
    pub(super) version: u32,
    pub(super) zones: Vec<PersistedZone>,
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
                start_mark_seen,
            },
            snapshot,
        )
    }

    fn retained_bytes(&self) -> usize {
        self.cmd
            .len()
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

/// Serialize a bounded session document.
pub(super) fn encode_session(zones: Vec<PersistedZone>) -> io::Result<Vec<u8>> {
    let session = PersistedZoneSession {
        version: FORMAT_VERSION,
        zones,
    };
    serde_json::to_vec(&session).map_err(|error| io::Error::other(error.to_string()))
}

/// Decode a session document, rejecting an unknown version outright rather
/// than replaying fields this build cannot interpret.
pub(super) fn decode_session(bytes: &[u8]) -> io::Result<Vec<PersistedZone>> {
    let session: PersistedZoneSession =
        serde_json::from_slice(bytes).map_err(|error| io::Error::other(error.to_string()))?;
    if session.version != FORMAT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported zone history version {} (expected {FORMAT_VERSION})",
                session.version
            ),
        ));
    }
    Ok(bound_persisted_zones(
        session.zones,
        MAX_RESTORED_ZONES,
        MAX_RESTORED_SNAPSHOT_BYTES,
    ))
}

/// Read a bounded zone-history file. A file over the ceiling is refused
/// without being decoded; a missing file is simply an empty session.
pub(super) fn read_session(path: &Path) -> io::Result<Vec<PersistedZone>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "zone history path is not a regular file",
        ));
    }
    if metadata.len() > MAX_ZONE_HISTORY_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "zone history file exceeds its bound",
        ));
    }
    decode_session(&std::fs::read(path)?)
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
        let budget = "first".len() + "second".len() + "third".len() + 64;
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
        let bounded = bound_persisted_zones(zones, 8, 5);
        assert_eq!(
            bounded.iter().map(|z| z.cmd.as_str()).collect::<Vec<_>>(),
            vec!["bbbb"]
        );
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
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
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

        let file = dir.join("zones.json");
        let encoded = encode_session(vec![zone("echo hi", Some("hi"))]).expect("encodes");
        std::fs::write(&file, &encoded).expect("write");
        let restored = read_session(&file).expect("reads");
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].output.as_deref(), Some("hi"));

        std::fs::write(&file, vec![b'x'; MAX_ZONE_HISTORY_FILE_BYTES as usize + 1]).expect("write");
        let error = read_session(&file).expect_err("refuses an oversized file");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let error = read_session(&dir).expect_err("refuses a directory");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restored_zones_take_fresh_ids_so_marker_authority_stays_monotonic() {
        let (record, snapshot) = zone("echo hi", Some("hi")).into_live(41);
        assert_eq!(record.id, 41, "the caller's id wins, not a persisted one");
        assert_eq!(snapshot.expect("snapshot").plain, "hi");
    }
}
