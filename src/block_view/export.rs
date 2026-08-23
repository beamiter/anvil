//! export — extracted from block_view (mechanical split, no logic changes)
//!
//! Serializes completed backend records to JSON / Markdown for the user-facing
//! export actions, plus a clipboard-copy helper for Block's right-click menu.
//! Block records include snapshotted output; a Unified record exports the
//! bounded snapshot retained beside it, and `output_available: false` with no
//! `output` key when none is retained.

use gtk::prelude::*;
use relm4::gtk;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;

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
        markdown.push_str(&format!("**Directory:** {cwd}\n\n"));
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
fn export_file_name(stamp: &str, extension: &str, attempt: u32) -> String {
    if attempt == 0 {
        format!("session-{stamp}.{extension}")
    } else {
        format!("session-{stamp}-{attempt}.{extension}")
    }
}

#[allow(dead_code)]
impl TermView {
    fn write_session_export(
        &self,
        writer: &mut impl Write,
        format: SessionExportFormat,
    ) -> io::Result<()> {
        let records = self.render_backend.records();
        match format {
            SessionExportFormat::Json => writer.write_all(records_json(&records).as_bytes()),
            SessionExportFormat::Markdown => {
                writeln!(writer, "# Terminal Session Export\n")?;
                writeln!(writer, "Total blocks: {}\n", records.len())?;
                writeln!(writer, "---\n")?;
                for (index, record) in records.iter().enumerate() {
                    writeln!(writer, "## Block #{}\n", index + 1)?;
                    writer.write_all(record_markdown(record).as_bytes())?;
                    writeln!(writer, "\n---\n")?;
                }
                Ok(())
            }
        }
    }

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

    /// Write the whole session's blocks to a timestamped file under the anvil
    /// data directory. Exports contain command output, so the file is created
    /// exclusively with owner-only permissions like the block history.
    pub fn export_session_to_file(&self, format: SessionExportFormat) -> io::Result<PathBuf> {
        let dir = gtk::glib::user_data_dir().join("anvil").join("exports");
        fs::create_dir_all(&dir)?;
        let stamp = gtk::glib::DateTime::now_local()
            .ok()
            .and_then(|now| now.format("%Y%m%d-%H%M%S").ok())
            .map(|formatted| formatted.to_string())
            .unwrap_or_else(|| format!("pid{}", std::process::id()));
        for attempt in 0..100u32 {
            let path = dir.join(export_file_name(&stamp, format.extension(), attempt));
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(mut file) => {
                    let result = self
                        .write_session_export(&mut file, format)
                        .and_then(|_| file.flush())
                        .and_then(|_| file.sync_all());
                    if let Err(error) = result {
                        drop(file);
                        let _ = fs::remove_file(&path);
                        return Err(error);
                    }
                    drop(file);
                    if let Ok(parent) = OpenOptions::new().read(true).open(&dir) {
                        let _ = parent.sync_all();
                    }
                    return Ok(path);
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "too many session exports share this timestamp",
        ))
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
        export_file_name, metadata_record_export, metadata_record_markdown, records_json,
        BackendRecords, SessionExportFormat,
    };
    use crate::block_view::{CompletedCommandRecord, UnifiedZoneStore, ZoneOutputSnapshot};
    use std::cell::RefCell;

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
