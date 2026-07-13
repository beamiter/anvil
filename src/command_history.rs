//! Lightweight, privacy-conscious command history.
//!
//! Full block snapshots are optional because they contain terminal output.
//! This JSONL index stores only the command, cwd, exit status, and completion
//! time so History, the command palette, and opt-in AI context work out of the
//! box without persisting command output.

use serde::Serialize;
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

const COMPACT_EVERY: u64 = 128;
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_RECORD_BYTES: usize = 1024 * 1024;
static APPEND_COUNT: AtomicU64 = AtomicU64::new(0);

#[derive(Serialize)]
struct CommandHistoryRecord<'a> {
    command: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<&'a str>,
    exit_code: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_time_ms: Option<u64>,
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
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }

    let record = CommandHistoryRecord {
        command,
        cwd,
        exit_code,
        end_time_ms,
    };
    let encoded = serde_json::to_vec(&record).map_err(io::Error::other)?;
    if encoded.len() > MAX_RECORD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "command history record exceeds 1 MiB",
        ));
    }

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
    file.write_all(b"\n")?;
    file.flush()?;

    let append_number = APPEND_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    let oversized = file.metadata()?.len() > MAX_FILE_BYTES;
    if oversized || append_number.is_multiple_of(COMPACT_EVERY) {
        compact(path, max_entries.max(1))?;
    }
    Ok(())
}

fn compact(path: &Path, max_entries: usize) -> io::Result<()> {
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
                let mut discard = Vec::new();
                reader.read_until(b'\n', &mut discard)?;
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

    let tmp = path.with_extension("jsonl.tmp");
    {
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let output = options.open(&tmp)?;
        let mut writer = BufWriter::new(output);
        for record in recent {
            writer.write_all(record.as_bytes())?;
        }
        writer.flush()?;
    }
    fs::rename(&tmp, path).or_else(|first| {
        fs::remove_file(path)?;
        fs::rename(&tmp, path).map_err(|second| {
            io::Error::new(
                second.kind(),
                format!("replace history failed: {first}; {second}"),
            )
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "jterm1-command-history-{name}-{}-{}.jsonl",
            std::process::id(),
            APPEND_COUNT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn append_writes_palette_compatible_jsonl() {
        let path = temp_path("append");
        append(&path, 100, "cargo test", Some("/tmp/project"), 0, Some(42)).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(value["command"], "cargo test");
        assert_eq!(value["cwd"], "/tmp/project");
        assert_eq!(value["exit_code"], 0);
        assert!(value.get("output").is_none());
        let _ = fs::remove_file(path);
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
        let _ = fs::remove_file(path);
    }
}
