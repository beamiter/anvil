//! Read-only comparison of two explicit, completed command identities.
//! GTK captures bounded strings; ANSI removal and alignment run off the UI thread.

use super::{BackendRecordRef, BlockState, TermView};
use gtk::prelude::*;
use jterm_core::output_compare::{compare, OutputSnapshot};
use relm4::{adw, gtk};
use std::collections::HashSet;
use std::time::Duration;

const MAX_SOURCE_BYTES: usize = 256 * 1024;
const MAX_METADATA_BYTES: usize = 4096;

#[derive(Debug)]
struct CapturedRecord {
    heading: String,
    output: String,
    ansi: bool,
    truncated: bool,
}

fn bounded_prefix(text: &str, limit: usize) -> (&str, bool) {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], end < text.len())
}

fn metadata(text: &str) -> String {
    crate::review_input::safe_inline_display(text, MAX_METADATA_BYTES)
}

fn start_label(start: Option<u64>) -> String {
    start
        .and_then(|ms| i64::try_from(ms / 1000).ok())
        .and_then(|seconds| gtk::glib::DateTime::from_unix_local(seconds).ok())
        .and_then(|time| time.format("%Y-%m-%d %H:%M:%S %Z").ok())
        .map(|text| text.to_string())
        .unwrap_or_else(|| "Unavailable".into())
}

fn capture_record(
    record: BackendRecordRef<'_>,
    role: &str,
) -> Result<CapturedRecord, &'static str> {
    let (id, command, cwd, exit, start, source, output, ansi, capture_limited) = match record {
        BackendRecordRef::Block(record) => {
            if record.is_background() {
                return Err(
                    "Choose two completed commands; background output has no command to compare.",
                );
            }
            (
                record.id,
                record.cmd.as_str(),
                record.cwd.as_deref(),
                record.exit_code,
                record
                    .timing_is_authoritative()
                    .then_some(record.start_time_ms)
                    .flatten(),
                if record.command_truncated {
                    "Command text shortened"
                } else if record.command_exact {
                    "Exact shell-reported command"
                } else {
                    "Captured command; exact text unverified"
                },
                record.output.as_str(),
                true,
                record.output_head_dropped,
            )
        }
        BackendRecordRef::Metadata { record, snapshot } => {
            if record.is_background {
                return Err(
                    "Choose two completed commands; background output has no command to compare.",
                );
            }
            let snapshot = snapshot.ok_or("One of these commands has no retained output snapshot. Its output cannot be compared.")?;
            (
                record.id,
                record.cmd.as_str(),
                record.cwd.as_deref(),
                record.exit_code,
                record
                    .timing_is_authoritative()
                    .then_some(record.start_time_ms)
                    .flatten(),
                match record.command_source {
                    super::CommandTextSource::ShellReported => "Shell-reported command",
                    super::CommandTextSource::ScreenAfterTruncation => "Command text shortened",
                    _ => "Screen-captured command; exact text unverified",
                },
                snapshot.plain.as_str(),
                false,
                snapshot.truncated,
            )
        }
    };
    let (output, source_limited) = bounded_prefix(output, MAX_SOURCE_BYTES);
    let exit = exit.map_or_else(|| "Unavailable".to_string(), |code| code.to_string());
    let command_note = if command.len() > MAX_METADATA_BYTES {
        " (display shortened)"
    } else {
        ""
    };
    let cwd = cwd.unwrap_or("Unavailable");
    let cwd_note = if cwd.len() > MAX_METADATA_BYTES {
        " (display shortened)"
    } else {
        ""
    };
    Ok(CapturedRecord {
        heading: format!(
            "{role} · Block #{id}\nStarted: {}\nCommand{command_note}: {}\nFolder{cwd_note}: {}\nExit: {exit} · {source}",
            start_label(start), metadata(command), metadata(cwd),
        ),
        output: output.to_string(),
        ansi,
        truncated: capture_limited || source_limited,
    })
}

fn capture_pair<'a>(
    records: impl IntoIterator<Item = BackendRecordRef<'a>>,
    ids: &HashSet<u64>,
) -> Result<[CapturedRecord; 2], &'static str> {
    if ids.len() != 2 {
        return Err("Select exactly two completed commands to compare their output.");
    }
    let mut selected = records
        .into_iter()
        .filter(|record| ids.contains(&record.id()));
    let missing = "A selected command is no longer retained. Select both commands again.";
    let before = selected.next().ok_or(missing)?;
    let after = selected.next().ok_or(missing)?;
    if before.id() == after.id() || selected.next().is_some() {
        return Err("The selected command identities are not unique. Select both commands again.");
    }
    Ok([
        capture_record(before, "Before")?,
        capture_record(after, "After")?,
    ])
}

fn previous_run_ids(
    records: &std::collections::VecDeque<super::CompletedCommandRecord>,
    id: u64,
) -> Result<[u64; 2], &'static str> {
    let position = records
        .iter()
        .position(|record| record.id == id)
        .ok_or("That command is no longer retained. Choose it again in Browse.")?;
    let current = &records[position];
    if current.is_background
        || current.cmd.trim().is_empty()
        || current.cmd == super::UNAVAILABLE_COMMAND_PLACEHOLDER
        || current.command_source == super::CommandTextSource::ScreenAfterTruncation
    {
        return Err(
            "Complete recorded command text is required to identify an earlier matching run.",
        );
    }
    let cwd = current.cwd.as_deref().filter(|cwd| !cwd.is_empty())
        .ok_or("The command's folder is unavailable, so an earlier run cannot be matched to the same folder.")?;
    let previous = records
        .iter()
        .take(position)
        .rev()
        .find(|record| {
            !record.is_background
                && record.cmd == current.cmd
                && record.cwd.as_deref() == Some(cwd)
                && record.command_source != super::CommandTextSource::ScreenAfterTruncation
        })
        .ok_or("No earlier retained run has the same command text and folder.")?;
    Ok([previous.id, current.id])
}

/// The caller supplies exact IDs, never a row index or newest-record fallback.
pub(super) fn show<'a>(
    parent: &gtk::Widget,
    records: impl IntoIterator<Item = BackendRecordRef<'a>>,
    ids: &HashSet<u64>,
) -> Result<(), &'static str> {
    use adw::prelude::*;
    let permit = super::export_policy::PreviewPermit::try_acquire()
        .ok_or("Another output comparison or export preview is still preparing. Try again when it finishes.")?;
    let [before, after] = capture_pair(records, ids)?;
    let dialog = adw::Dialog::builder()
        .title("Compare command output")
        .content_width(900)
        .content_height(640)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    body.set_margin_start(12);
    body.set_margin_end(12);
    body.set_margin_top(12);
    body.set_margin_bottom(12);
    let context = gtk::Label::new(Some(&format!("{}\n\n{}", before.heading, after.heading)));
    context.set_wrap(true);
    context.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    context.set_xalign(0.0);
    context.set_selectable(true);
    let context_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .max_content_height(190)
        .propagate_natural_height(true)
        .child(&context)
        .build();
    body.append(&context_scroll);
    let status = gtk::Label::new(Some("Comparing retained text…"));
    status.set_wrap(true);
    status.set_xalign(0.0);
    status.set_accessible_role(gtk::AccessibleRole::Status);
    body.append(&status);
    let output = gtk::TextView::new();
    output.set_editable(false);
    output.set_cursor_visible(false);
    output.set_monospace(true);
    output.set_wrap_mode(gtk::WrapMode::None);
    output.set_left_margin(8);
    output.set_right_margin(8);
    body.append(
        &gtk::ScrolledWindow::builder()
            .vexpand(true)
            .child(&output)
            .build(),
    );
    let copy = gtk::Button::with_label("Copy displayed comparison");
    copy.set_sensitive(false);
    body.append(&copy);
    toolbar.set_content(Some(&body));
    dialog.set_child(Some(&toolbar));
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let closed = cancelled.clone();
    dialog.connect_closed(move |_| {
        closed.store(true, std::sync::atomic::Ordering::Release);
    });
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("output-compare".into())
        .spawn(move || {
            let _permit = permit;
            if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            let before_text = if before.ansi {
                super::strip_ansi(&before.output)
            } else {
                before.output
            };
            let after_text = if after.ansi {
                super::strip_ansi(&after.output)
            } else {
                after.output
            };
            let result = compare(
                OutputSnapshot {
                    text: &before_text,
                    truncated: before.truncated,
                },
                OutputSnapshot {
                    text: &after_text,
                    truncated: after.truncated,
                },
            );
            if !cancelled.load(std::sync::atomic::Ordering::Acquire) {
                let _ = tx.send(result);
            }
        })
        .map_err(|_| "The output comparison worker could not start. Try again.")?;
    let copy_output = output.downgrade();
    let copy_context = context.downgrade();
    let copy_status = status.downgrade();
    copy.connect_clicked(move |button| {
        if !button.is_sensitive() {
            return;
        }
        if let (Some(output), Some(context), Some(status)) = (
            copy_output.upgrade(),
            copy_context.upgrade(),
            copy_status.upgrade(),
        ) {
            let buffer = output.buffer();
            button.clipboard().set_text(&format!(
                "{}\n\n{}\n\n{}",
                context.text(),
                status.text(),
                buffer.text(&buffer.start_iter(), &buffer.end_iter(), false),
            ));
        }
    });
    let weak_dialog = dialog.downgrade();
    gtk::glib::timeout_add_local(Duration::from_millis(40), move || {
        let Some(dialog) = weak_dialog.upgrade() else {
            return gtk::glib::ControlFlow::Break;
        };
        if !dialog.is_visible() {
            return gtk::glib::ControlFlow::Break;
        }
        match rx.try_recv() {
            Ok(result) => {
                status.set_text(&format!(
                    "{} · Retained text only; images and terminal layout are not compared.",
                    result.summary()
                ));
                output.buffer().set_text(&result.text);
                copy.set_sensitive(true);
                gtk::glib::ControlFlow::Break
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => gtk::glib::ControlFlow::Continue,
            Err(_) => {
                status.set_text("The comparison worker stopped. Close this view and try again.");
                gtk::glib::ControlFlow::Break
            }
        }
    });
    dialog.present(Some(parent));
    Ok(())
}

impl TermView {
    pub(crate) fn compare_command_output(&self) -> Result<(), &'static str> {
        if self.unified_navigation.is_some() {
            return self.compare_previous_unified_run();
        }
        if self.fullscreen.get() || self.bstate.get() == BlockState::AltScreen {
            return Err("The full-screen application owns this terminal.");
        }
        let ids = self.selected_block_ids.borrow().clone();
        show(
            self.root.upcast_ref(),
            self.render_backend.records().iter(),
            &ids,
        )
    }

    /// Match raw command and folder identity, then choose the nearest earlier
    /// retained run. The result always labels both selected stable identities.
    pub(super) fn compare_previous_unified_run(&self) -> Result<(), &'static str> {
        if self.unified_navigation.is_none() {
            return Err("Choose two completed cards and open Review to compare output.");
        }
        if self.fullscreen.get() || self.bstate.get() == BlockState::AltScreen {
            return Err("The full-screen application owns this terminal.");
        }
        let id = self
            .navigated_record_id
            .get()
            .ok_or("Choose a completed command with Previous, Next or Browse first.")?;
        let records = self.render_backend.records();
        let super::BackendRecords::Metadata(store) = &records else {
            unreachable!();
        };
        let ids = previous_run_ids(&store.records, id)?;
        show(self.root.upcast_ref(), records.iter(), &HashSet::from(ids))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: u64, command: &str, output: &str) -> super::super::BlockData {
        serde_json::from_value(serde_json::json!({
            "id": id, "prompt": "$ ", "cmd": command, "output": output,
            "exit_code": 0, "estimated_height": 20, "line_count": 1,
            "cwd": "/workspace/project", "command_exact": true,
        }))
        .unwrap()
    }

    #[test]
    fn comparison_resolves_exact_ids_in_terminal_order_and_rejects_partial_selection() {
        let records = [record(30, "build", "old"), record(2, "build", "new")];
        let pair = capture_pair(
            records.iter().map(BackendRecordRef::Block),
            &HashSet::from([2, 30]),
        )
        .unwrap();
        assert!(pair[0].heading.starts_with("Before · Block #30"));
        assert!(pair[1].heading.starts_with("After · Block #2"));
        assert_eq!(pair[0].output, "old");
        assert!(capture_pair(
            records.iter().map(BackendRecordRef::Block),
            &HashSet::from([2, 99])
        )
        .is_err());
        assert!(capture_pair(
            records.iter().map(BackendRecordRef::Block),
            &HashSet::from([2])
        )
        .is_err());
    }

    #[test]
    fn captured_output_is_utf8_bounded_and_preserves_capture_loss() {
        let mut data = record(1, "build", &"界".repeat(MAX_SOURCE_BYTES));
        let capture = capture_record(BackendRecordRef::Block(&data), "Before").unwrap();
        assert!(capture.output.len() <= MAX_SOURCE_BYTES && capture.truncated);
        data.output = "".into();
        data.output_head_dropped = true;
        assert!(
            capture_record(BackendRecordRef::Block(&data), "Before")
                .unwrap()
                .truncated
        );
        assert!(capture_record(
            BackendRecordRef::Block(&record(2, "", "background")),
            "After"
        )
        .is_err());
    }

    #[test]
    fn missing_unified_snapshot_cannot_pose_as_empty_output() {
        let record = super::super::CompletedCommandRecord {
            id: 1,
            cmd: "cargo test".into(),
            exit_code: Some(0),
            start_time_ms: None,
            end_time_ms: None,
            duration_ms: None,
            cwd: Some("/workspace/project".into()),
            is_background: false,
            completion_provenance: super::super::CompletionProvenance::ShellReported,
            command_source: super::super::CommandTextSource::ShellReported,
            start_mark_seen: true,
        };
        assert!(capture_record(
            BackendRecordRef::Metadata {
                record: &record,
                snapshot: None
            },
            "Before"
        )
        .is_err());
        let empty = super::super::ZoneOutputSnapshot {
            plain: String::new(),
            truncated: false,
        };
        let capture = capture_record(
            BackendRecordRef::Metadata {
                record: &record,
                snapshot: Some(&empty),
            },
            "Before",
        )
        .unwrap();
        assert!(capture.output.is_empty() && !capture.truncated);
        let mut previous = record.clone();
        previous.id = 30;
        let mut other_folder = record.clone();
        other_folder.id = 20;
        other_folder.cwd = Some("/workspace/elsewhere".into());
        let mut history = std::collections::VecDeque::from([previous, other_folder, record]);
        assert_eq!(previous_run_ids(&history, 1), Ok([30, 1]));
        assert!(previous_run_ids(&history, 99).is_err());
        history[2].cwd = None;
        history[0].cwd = None;
        assert!(
            previous_run_ids(&history, 1).is_err(),
            "unknown folders cannot compare equal"
        );
    }
}
