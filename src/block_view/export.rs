//! export — extracted from block_view (mechanical split, no logic changes)
//!
//! Serializes completed backend records to JSON / Markdown for the user-facing
//! export actions, plus a clipboard-copy helper for Block's right-click menu.
//! Block records include snapshotted output; a Unified record exports the
//! bounded snapshot retained beside it, and `output_available: false` with no
//! `output` key when none is retained.

use super::session_export_io::{self, Cancellation};
use gtk::prelude::*;
use relm4::gtk;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use super::{
    markdown_fence, strip_ansi, BackendRecordRef, BackendRecords, CompletedCommandRecord, TermView,
    ZoneOutputSnapshot, MAX_ZONE_SNAPSHOT_BYTES,
};

#[derive(serde::Serialize)]
struct MetadataRecordExport<'a> {
    id: u64,
    cmd: &'a str,
    exit_code: Option<i32>,
    start_time_ms: Option<u64>,
    end_time_ms: Option<u64>,
    duration_ms: Option<u64>,
    cwd: Option<&'a str>,
    is_background: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    completion_provenance: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lifecycle_health: Option<&'a str>,
    /// The bounded finalize-time snapshot. Omitted — never an empty string —
    /// when no snapshot is retained (none was captured, or the global budget
    /// evicted it).
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<&'a str>,
    /// `Some` exactly when `output` is; `true` when the snapshot holds only
    /// part of the command's output.
    #[serde(skip_serializing_if = "Option::is_none")]
    output_truncated: Option<bool>,
    output_available: bool,
}

fn metadata_record_export<'a>(
    record: &'a CompletedCommandRecord,
    snapshot: Option<&'a ZoneOutputSnapshot>,
) -> MetadataRecordExport<'a> {
    MetadataRecordExport {
        id: record.id,
        cmd: &record.cmd,
        exit_code: record.exit_code,
        start_time_ms: record
            .timing_is_authoritative()
            .then_some(record.start_time_ms)
            .flatten(),
        end_time_ms: record
            .timing_is_authoritative()
            .then_some(record.end_time_ms)
            .flatten(),
        duration_ms: record
            .timing_is_authoritative()
            .then_some(record.duration_ms)
            .flatten(),
        cwd: record.cwd.as_deref(),
        is_background: record.is_background,
        completion_provenance: (!record.is_background)
            .then_some(record.completion_provenance.schema_name()),
        lifecycle_health: (!record.is_background)
            .then_some(record.lifecycle_health().schema_name()),
        output: snapshot.map(|snapshot| snapshot.plain.as_str()),
        output_truncated: snapshot.map(|snapshot| snapshot.truncated),
        output_available: snapshot.is_some(),
    }
}

fn metadata_record_markdown(
    record: &CompletedCommandRecord,
    snapshot: Option<&ZoneOutputSnapshot>,
) -> String {
    let mut markdown = if record.is_background {
        "## Background Output\n\n".to_string()
    } else {
        let fence = markdown_fence(&record.cmd);
        format!(
            "## Command Record\n\n**Command:**\n{fence}bash\n{}\n{fence}\n\n",
            record.cmd
        )
    };
    match snapshot {
        Some(snapshot) => {
            markdown.push_str("**Output:**");
            if snapshot.truncated {
                markdown.push_str(&format!(
                    " (truncated to the last {} KiB)",
                    MAX_ZONE_SNAPSHOT_BYTES / 1024
                ));
            }
            let fence = markdown_fence(&snapshot.plain);
            markdown.push_str(&format!("\n{fence}\n{}\n{fence}\n\n", snapshot.plain));
        }
        None => markdown.push_str(
            "**Output:** unavailable (retained on the live Unified terminal surface only)\n\n",
        ),
    }
    if !record.is_background {
        match record.exit_code {
            Some(code) => markdown.push_str(&format!("**Exit Code:** {code}\n\n")),
            None => markdown.push_str("**Exit Code:** unknown (the shell reported none)\n\n"),
        }
        markdown.push_str(&format!(
            "**Lifecycle:** {} ({})\n\n",
            record.lifecycle_health().schema_name(),
            record.completion_provenance.schema_name(),
        ));
    }
    if let Some(duration_ms) = record
        .timing_is_authoritative()
        .then_some(record.duration_ms)
        .flatten()
    {
        markdown.push_str(&format!(
            "**Duration:** {:.3}s\n\n",
            duration_ms as f64 / 1_000.0
        ));
    }
    // Same reproduction context Block's own card export carries.
    if let Some(cwd) = record.cwd.as_deref().filter(|cwd| !cwd.is_empty()) {
        // OSC-provided paths can contain Markdown links, images, HTML, or
        // backticks. Keep the original bytes inert inside their own fence.
        let fence = markdown_fence(cwd);
        markdown.push_str(&format!("**Directory:**\n{fence}\n{cwd}\n{fence}\n\n"));
    }
    markdown
}

fn record_json(record: BackendRecordRef<'_>) -> String {
    match record {
        BackendRecordRef::Block(record) => record.to_json(),
        BackendRecordRef::Metadata { record, snapshot } => {
            serde_json::to_string_pretty(&metadata_record_export(record, snapshot))
                .unwrap_or_else(|_| "{}".to_string())
        }
    }
}

fn record_markdown(record: BackendRecordRef<'_>) -> String {
    match record {
        BackendRecordRef::Block(record) => record.to_markdown(),
        BackendRecordRef::Metadata { record, snapshot } => {
            metadata_record_markdown(record, snapshot)
        }
    }
}

fn records_json(records: &BackendRecords<'_>) -> String {
    match records {
        BackendRecords::Blocks(records) => {
            serde_json::to_string_pretty(&**records).unwrap_or_else(|_| "[]".to_string())
        }
        BackendRecords::Metadata(store) => {
            let exports: Vec<_> = store
                .records
                .iter()
                .map(|record| metadata_record_export(record, store.snapshot(record.id)))
                .collect();
            serde_json::to_string_pretty(&exports).unwrap_or_else(|_| "[]".to_string())
        }
    }
}

/// On-disk formats for whole-session export.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionExportFormat {
    Markdown,
    Json,
}

impl SessionExportFormat {
    fn extension(self) -> &'static str {
        match self {
            SessionExportFormat::Markdown => "md",
            SessionExportFormat::Json => "json",
        }
    }
}

/// `session-<stamp>.<ext>` with a numeric suffix for same-second collisions.
#[cfg(test)]
fn export_file_name(stamp: &str, extension: &str, attempt: u32) -> String {
    if attempt == 0 {
        format!("session-{stamp}.{extension}")
    } else {
        format!("session-{stamp}-{attempt}.{extension}")
    }
}

#[cfg(test)]
type TestSync = Box<dyn FnOnce(&std::fs::File) -> io::Result<()> + Send>;
#[cfg(test)]
thread_local! {
    static TEST_SYNC: std::cell::RefCell<Option<TestSync>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
pub(crate) fn set_test_sync(hook: impl FnOnce(&std::fs::File) -> io::Result<()> + Send + 'static) {
    TEST_SYNC.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

const MAX_EXPORT_RECORDS: usize = 4096;
const MAX_EXPORT_BYTES: usize = 16 * 1024 * 1024;

enum OwnedRecord {
    Block(super::BlockData),
    Metadata(CompletedCommandRecord, Option<ZoneOutputSnapshot>),
}
impl OwnedRecord {
    fn borrowed(&self) -> BackendRecordRef<'_> {
        match self {
            Self::Block(record) => BackendRecordRef::Block(record),
            Self::Metadata(record, snapshot) => BackendRecordRef::Metadata {
                record,
                snapshot: snapshot.as_ref(),
            },
        }
    }
}
impl serde::Serialize for OwnedRecord {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Block(record) => record.serialize(serializer),
            Self::Metadata(record, snapshot) => {
                metadata_record_export(record, snapshot.as_ref()).serialize(serializer)
            }
        }
    }
}

/// Future range/preview selection can consume this boundary without GTK in the worker.
pub(crate) struct SessionExportSnapshot(Vec<OwnedRecord>);
impl SessionExportSnapshot {
    fn capture(records: &BackendRecords<'_>) -> io::Result<Self> {
        let too_large = || {
            io::Error::other("session exceeds export snapshot budget (4096 records / 16 MiB); export individual blocks instead")
        };
        if records.len() > MAX_EXPORT_RECORDS {
            return Err(too_large());
        }
        let mut bytes = records
            .len()
            .saturating_mul(std::mem::size_of::<OwnedRecord>());
        // Size every string before cloning any record. This is O(record count),
        // not O(output bytes), and bounds both GTK copy work and worker memory.
        for record in records.iter() {
            let sizes = match record {
                BackendRecordRef::Block(r) => [
                    r.prompt.len(),
                    r.cmd.len(),
                    r.output.len(),
                    r.cmd_markup.as_ref().map_or(0, String::len),
                    r.cwd.as_ref().map_or(0, String::len),
                ],
                BackendRecordRef::Metadata {
                    record: r,
                    snapshot,
                } => [
                    r.cmd.len(),
                    r.cwd.as_ref().map_or(0, String::len),
                    snapshot.map_or(0, |s| s.plain.len()),
                    0,
                    0,
                ],
            };
            for size in sizes {
                bytes = bytes.checked_add(size).ok_or_else(too_large)?;
            }
            if bytes > MAX_EXPORT_BYTES {
                return Err(too_large());
            }
        }
        Ok(Self(
            records
                .iter()
                .map(|record| match record {
                    BackendRecordRef::Block(r) => OwnedRecord::Block(r.clone()),
                    BackendRecordRef::Metadata { record, snapshot } => {
                        OwnedRecord::Metadata(record.clone(), snapshot.cloned())
                    }
                })
                .collect(),
        ))
    }

    fn write(
        &self,
        writer: &mut dyn Write,
        format: SessionExportFormat,
        completed: &AtomicUsize,
    ) -> io::Result<()> {
        match format {
            SessionExportFormat::Json => {
                // SerializeSeq retains serde_json's exact pretty-array format.
                use serde::ser::{SerializeSeq, Serializer};
                let mut serializer = serde_json::Serializer::pretty(writer);
                let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
                for record in &self.0 {
                    sequence.serialize_element(record)?;
                    completed.fetch_add(1, Ordering::Relaxed);
                }
                sequence.end().map_err(io::Error::other)
            }
            SessionExportFormat::Markdown => {
                writeln!(writer, "# Terminal Session Export\n")?;
                writeln!(writer, "Total blocks: {}\n", self.0.len())?;
                writeln!(writer, "---\n")?;
                for (index, record) in self.0.iter().enumerate() {
                    writeln!(writer, "## Block #{}\n", index + 1)?;
                    writer.write_all(record_markdown(record.borrowed()).as_bytes())?;
                    writeln!(writer, "\n---\n")?;
                    completed.fetch_add(1, Ordering::Relaxed);
                }
                Ok(())
            }
        }
    }

    pub(crate) fn start(self, format: SessionExportFormat) -> io::Result<SessionExportJob> {
        let directory = gtk::glib::user_data_dir().join("anvil").join("exports");
        let stamp = gtk::glib::DateTime::now_local()
            .ok()
            .and_then(|now| now.format("%Y%m%d-%H%M%S").ok())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("pid{}", std::process::id()));
        let cancellation = Cancellation::default();
        let completed = Arc::new(AtomicUsize::new(0));
        let total = self.0.len();
        let (tx, result) = std::sync::mpsc::sync_channel(1);
        let cancel = cancellation.clone();
        let progress = completed.clone();
        #[cfg(test)]
        let sync_hook = TEST_SYNC.with(|slot| slot.borrow_mut().take());
        std::thread::Builder::new()
            .name("session-export".into())
            .spawn(move || {
                let write = |writer: &mut dyn Write| self.write(writer, format, &progress);
                #[cfg(test)]
                let outcome = if let Some(sync) = sync_hook {
                    session_export_io::publish_with_sync(
                        &directory,
                        &stamp,
                        format.extension(),
                        &cancel,
                        write,
                        sync,
                    )
                } else {
                    session_export_io::publish(
                        &directory,
                        &stamp,
                        format.extension(),
                        &cancel,
                        write,
                    )
                };
                #[cfg(not(test))]
                let outcome = session_export_io::publish(
                    &directory,
                    &stamp,
                    format.extension(),
                    &cancel,
                    write,
                );
                let _ = tx.send(outcome);
            })?;
        Ok(SessionExportJob {
            cancellation,
            completed,
            total,
            result,
        })
    }
}
pub(crate) struct SessionExportJob {
    pub(crate) cancellation: Cancellation,
    pub(crate) completed: Arc<AtomicUsize>,
    pub(crate) total: usize,
    pub(crate) result: std::sync::mpsc::Receiver<io::Result<PathBuf>>,
}
impl Drop for SessionExportJob {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

#[allow(dead_code)]
impl TermView {
    /// Export a block by ID to JSON format
    pub fn export_block_json(&self, block_id: u64) -> Option<String> {
        let records = self.render_backend.records();
        records
            .iter()
            .find(|record| record.id() == block_id)
            .map(record_json)
    }

    /// Export a block by ID to Markdown format
    pub fn export_block_markdown(&self, block_id: u64) -> Option<String> {
        let records = self.render_backend.records();
        records
            .iter()
            .find(|record| record.id() == block_id)
            .map(record_markdown)
    }

    /// Export all blocks in the session as JSON
    pub fn export_session_json(&self) -> String {
        let records = self.render_backend.records();
        records_json(&records)
    }

    /// Export all blocks in the session as Markdown
    pub fn export_session_markdown(&self) -> String {
        let records = self.render_backend.records();
        let mut md = String::new();

        md.push_str("# Terminal Session Export\n\n");
        md.push_str(&format!("Total blocks: {}\n\n", records.len()));
        md.push_str("---\n\n");

        for (index, record) in records.iter().enumerate() {
            md.push_str(&format!("## Block #{}\n\n", index + 1));
            md.push_str(&record_markdown(record));
            md.push_str("\n---\n\n");
        }

        md
    }

    /// Capture only owned, bounded data on GTK's thread. All formatting and I/O
    /// run in the worker. Oversized sessions fail rather than silently truncate.
    pub(crate) fn session_export_snapshot(&self) -> io::Result<SessionExportSnapshot> {
        SessionExportSnapshot::capture(&self.render_backend.records())
    }

    /// Copy a block's content to clipboard (prompt + cmd + output).
    pub fn copy_block_by_id(&self, block_id: u64) {
        let finished = self.finished_blocks.borrow();
        if let Some(block) = finished.iter().find(|b| b.id == block_id) {
            let prompt_text = block.prompt_text.clone();
            let cmd_text = block.cmd_text.clone();
            let output_text = strip_ansi(&block.full_output.borrow());

            let full_text = format!("{}\n{}\n{}", prompt_text, cmd_text, output_text);
            let clipboard = self.active_vte.clipboard();
            clipboard.set_text(&full_text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        export_file_name, markdown_fence, metadata_record_export, metadata_record_markdown,
        records_json, BackendRecords, SessionExportFormat,
    };
    use crate::block_view::{CompletedCommandRecord, UnifiedZoneStore, ZoneOutputSnapshot};
    use std::cell::RefCell;

    fn assert_snapshot_parity(records: &BackendRecords<'_>) {
        let snapshot = super::SessionExportSnapshot::capture(records).unwrap();
        let mut json = Vec::new();
        snapshot
            .write(
                &mut json,
                SessionExportFormat::Json,
                &super::AtomicUsize::new(0),
            )
            .unwrap();
        assert_eq!(json, records_json(records).as_bytes());
        let mut expected = format!(
            "# Terminal Session Export\n\nTotal blocks: {}\n\n---\n\n",
            records.len()
        );
        for (i, record) in records.iter().enumerate() {
            expected.push_str(&format!("## Block #{}\n\n", i + 1));
            expected.push_str(&super::record_markdown(record));
            expected.push_str("\n---\n\n");
        }
        let mut markdown = Vec::new();
        snapshot
            .write(
                &mut markdown,
                SessionExportFormat::Markdown,
                &super::AtomicUsize::new(0),
            )
            .unwrap();
        assert_eq!(markdown, expected.as_bytes());
    }

    #[test]
    fn snapshot_stream_preserves_exact_json_and_markdown_bytes() {
        let blocks = RefCell::new(std::collections::VecDeque::new());
        assert_snapshot_parity(&BackendRecords::Blocks(blocks.borrow()));
        let block: crate::block_view::BlockData = serde_json::from_value(serde_json::json!({
            "id": 1, "prompt": "user 中文", "cmd": "printf '```'", "cmd_markup": "<b>cmd</b>",
            "output": "```\noutput 🦀", "exit_code": 1, "estimated_height": 20, "line_count": 2,
            "cwd": "/tmp/![host](example.invalid)", "start_time_ms": 10, "end_time_ms": 20,
            "duration_ms": 10
        }))
        .unwrap();
        blocks.borrow_mut().push_back(block);
        assert_snapshot_parity(&BackendRecords::Blocks(blocks.borrow()));
        let mut store = UnifiedZoneStore::new();
        assert_snapshot_parity(&BackendRecords::Metadata(RefCell::new(store).borrow()));
        store = UnifiedZoneStore::new();
        store.records.push_back(metadata_record(1));
        let mut degraded = metadata_record(2);
        degraded.completion_provenance = crate::block_view::CompletionProvenance::BoundaryInferred;
        store.records.push_back(degraded);
        store.insert_snapshot(
            1,
            ZoneOutputSnapshot {
                plain: "```\nretained 中文".into(),
                truncated: true,
            },
        );
        let store = RefCell::new(store);
        assert_snapshot_parity(&BackendRecords::Metadata(store.borrow()));
        // Capture remains independent of later live-store mutation/session changes.
        let snapshot =
            super::SessionExportSnapshot::capture(&BackendRecords::Metadata(store.borrow()))
                .unwrap();
        let before = records_json(&BackendRecords::Metadata(store.borrow()));
        store.borrow_mut().records.clear();
        let mut bytes = Vec::new();
        snapshot
            .write(
                &mut bytes,
                SessionExportFormat::Json,
                &super::AtomicUsize::new(0),
            )
            .unwrap();
        assert_eq!(bytes, before.as_bytes());
    }

    #[test]
    fn snapshot_budget_rejects_before_cloning_without_truncating() {
        let mut store = UnifiedZoneStore::new();
        let mut record = metadata_record(1);
        record.cmd = "x".repeat(super::MAX_EXPORT_BYTES);
        store.records.push_back(record);
        let store = RefCell::new(store);
        assert!(
            super::SessionExportSnapshot::capture(&BackendRecords::Metadata(store.borrow()))
                .is_err()
        );
        store.borrow_mut().records.clear();
        for id in 0..=super::MAX_EXPORT_RECORDS {
            store
                .borrow_mut()
                .records
                .push_back(metadata_record(id as u64));
        }
        assert!(
            super::SessionExportSnapshot::capture(&BackendRecords::Metadata(store.borrow()))
                .is_err()
        );
    }

    #[test]
    fn export_file_names_disambiguate_same_second_collisions() {
        assert_eq!(
            export_file_name(
                "20260725-101112",
                SessionExportFormat::Markdown.extension(),
                0
            ),
            "session-20260725-101112.md"
        );
        assert_eq!(
            export_file_name("20260725-101112", SessionExportFormat::Json.extension(), 2),
            "session-20260725-101112-2.json"
        );
    }

    fn metadata_record(id: u64) -> CompletedCommandRecord {
        CompletedCommandRecord {
            id,
            cmd: "cargo test".to_string(),
            exit_code: Some(1),
            start_time_ms: Some(10),
            end_time_ms: Some(25),
            duration_ms: Some(15),
            cwd: Some("/work".to_string()),
            is_background: false,
            completion_provenance: crate::block_view::CompletionProvenance::ShellReported,
            command_source: crate::block_view::CommandTextSource::ShellReported,
            start_mark_seen: true,
        }
    }

    /// A record with no retained snapshot must never export an empty output:
    /// the key is absent and `output_available` says so.
    #[test]
    fn unified_metadata_export_marks_output_unavailable_instead_of_empty() {
        let record = metadata_record(7);
        let json = serde_json::to_value(metadata_record_export(&record, None)).unwrap();
        assert_eq!(json["output_available"], false);
        assert!(json.get("output").is_none());
        assert!(json.get("output_truncated").is_none());
        assert_eq!(json["cmd"], "cargo test");

        let markdown = metadata_record_markdown(&record, None);
        assert!(markdown.contains("retained on the live Unified terminal surface only"));
        assert!(markdown.contains("**Exit Code:** 1"));
    }

    #[test]
    fn inferred_completion_exports_degraded_without_fabricated_timing() {
        let mut record = metadata_record(12);
        record.exit_code = None;
        record.completion_provenance = crate::block_view::CompletionProvenance::BoundaryInferred;
        // Even a hand-edited/older persistence record cannot smuggle timing
        // back onto a source which did not report its completion boundary.
        record.end_time_ms = Some(25);
        record.duration_ms = Some(15);
        let export = serde_json::to_value(metadata_record_export(&record, None)).unwrap();
        assert_eq!(export["completion_provenance"], "boundary_inferred");
        assert_eq!(export["lifecycle_health"], "degraded");
        assert_eq!(export["end_time_ms"], serde_json::Value::Null);
        assert_eq!(export["duration_ms"], serde_json::Value::Null);
        let markdown = metadata_record_markdown(&record, None);
        assert!(markdown.contains("**Lifecycle:** degraded (boundary_inferred)"));
        assert!(!markdown.contains("**Duration:**"));
    }

    #[test]
    fn unified_metadata_export_carries_a_retained_snapshot_and_its_truncation() {
        let record = metadata_record(8);
        let snapshot = ZoneOutputSnapshot {
            plain: "running 3 tests\ntest result: ok".to_string(),
            truncated: false,
        };
        let json = serde_json::to_value(metadata_record_export(&record, Some(&snapshot))).unwrap();
        assert_eq!(json["output"], "running 3 tests\ntest result: ok");
        assert_eq!(json["output_truncated"], false);
        assert_eq!(json["output_available"], true);
        assert!(metadata_record_markdown(&record, Some(&snapshot))
            .contains("**Output:**\n```\nrunning 3 tests\ntest result: ok\n```\n\n"));

        let truncated = ZoneOutputSnapshot {
            plain: "…last lines".to_string(),
            truncated: true,
        };
        let json = serde_json::to_value(metadata_record_export(&record, Some(&truncated))).unwrap();
        assert_eq!(json["output_truncated"], true);
        assert!(metadata_record_markdown(&record, Some(&truncated))
            .contains("**Output:** (truncated to the last 64 KiB)\n```\n…last lines\n```\n\n"));
    }

    /// Snapshot text is untrusted command output: a fence it could close would
    /// let it escape its own block and forge document structure.
    #[test]
    fn snapshot_markdown_fence_outlives_backticks_in_the_output() {
        let mut record = metadata_record(9);
        record.cmd = "printf '```'".to_string();
        let snapshot = ZoneOutputSnapshot {
            plain: "```\n## not a document heading".to_string(),
            truncated: false,
        };
        let markdown = metadata_record_markdown(&record, Some(&snapshot));
        assert!(markdown.contains("**Command:**\n````bash\nprintf '```'\n````\n\n"));
        assert!(markdown.contains("**Output:**\n````\n```\n## not a document heading\n````\n\n"));
    }

    #[test]
    fn metadata_markdown_keeps_directory_syntax_inside_a_code_fence() {
        for cwd in [
            "/work/![image](https://example.invalid/pixel)",
            "/work/<img src='https://example.invalid/pixel'>",
            "/work/```\n## fabricated heading",
            "/work/中文 🦀",
        ] {
            let mut record = metadata_record(13);
            record.cwd = Some(cwd.to_string());
            let markdown = metadata_record_markdown(&record, None);
            let fence = markdown_fence(cwd);
            assert!(
                markdown.ends_with(&format!("**Directory:**\n{fence}\n{cwd}\n{fence}\n\n")),
                "{markdown}"
            );
            let json = serde_json::to_value(metadata_record_export(&record, None)).unwrap();
            assert_eq!(json["cwd"], cwd);
        }
    }

    /// Budget eviction removes only snapshot bytes; the surviving record must
    /// export exactly like one that never had output retained.
    #[test]
    fn budget_evicted_snapshot_exports_as_unavailable_again() {
        let mut store = UnifiedZoneStore::new();
        store.records.push_back(metadata_record(90));
        store.insert_snapshot(
            90,
            ZoneOutputSnapshot {
                plain: "compiling".to_string(),
                truncated: false,
            },
        );

        let retained = RefCell::new(store);
        let value: serde_json::Value =
            serde_json::from_str(&records_json(&BackendRecords::Metadata(retained.borrow())))
                .expect("valid metadata export JSON");
        assert_eq!(value[0]["output"], "compiling");

        retained.borrow_mut().enforce_snapshot_budget(0);
        let value: serde_json::Value =
            serde_json::from_str(&records_json(&BackendRecords::Metadata(retained.borrow())))
                .expect("valid metadata export JSON");
        let object = value[0].as_object().expect("one exported record");
        assert_eq!(object.get("cmd"), Some(&serde_json::json!("cargo test")));
        assert_eq!(
            object.get("output_available"),
            Some(&serde_json::json!(false))
        );
        assert!(!object.contains_key("output"));
        assert!(!object.contains_key("output_truncated"));
    }
}
