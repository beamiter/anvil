//! export — extracted from block_view (mechanical split, no logic changes)
//!
//! Serializes finished blocks to JSON / Markdown for the user-facing export
//! actions, plus a clipboard-copy helper for the per-block right-click menu.
//! Reads only the in-memory `block_data` and `finished_blocks` snapshots; no
//! VTE state mutation.

use gtk::prelude::*;
use relm4::gtk;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;

use super::{strip_ansi, BlockData, TermView};

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
        let blocks = self.block_data.borrow();
        match format {
            SessionExportFormat::Json => serde_json::to_writer_pretty(writer, &*blocks)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
            SessionExportFormat::Markdown => {
                writeln!(writer, "# Terminal Session Export\n")?;
                writeln!(writer, "Total blocks: {}\n", blocks.len())?;
                writeln!(writer, "---\n")?;
                for (index, block) in blocks.iter().enumerate() {
                    writeln!(writer, "## Block #{}\n", index + 1)?;
                    writer.write_all(block.to_markdown().as_bytes())?;
                    writeln!(writer, "\n---\n")?;
                }
                Ok(())
            }
        }
    }

    /// Export a block by ID to JSON format
    pub fn export_block_json(&self, block_id: u64) -> Option<String> {
        let blocks = self.block_data.borrow();
        blocks
            .iter()
            .find(|b| b.id == block_id)
            .map(|b| b.to_json())
    }

    /// Export a block by ID to Markdown format
    pub fn export_block_markdown(&self, block_id: u64) -> Option<String> {
        let blocks = self.block_data.borrow();
        blocks
            .iter()
            .find(|b| b.id == block_id)
            .map(|b| b.to_markdown())
    }

    /// Export all blocks in the session as JSON
    pub fn export_session_json(&self) -> String {
        let blocks = self.block_data.borrow();
        let blocks_vec: Vec<&BlockData> = blocks.iter().collect();
        serde_json::to_string_pretty(&blocks_vec).unwrap_or_else(|_| "[]".to_string())
    }

    /// Export all blocks in the session as Markdown
    pub fn export_session_markdown(&self) -> String {
        let blocks = self.block_data.borrow();
        let mut md = String::new();

        md.push_str("# Terminal Session Export\n\n");
        md.push_str(&format!("Total blocks: {}\n\n", blocks.len()));
        md.push_str("---\n\n");

        for (index, block) in blocks.iter().enumerate() {
            md.push_str(&format!("## Block #{}\n\n", index + 1));
            md.push_str(&block.to_markdown());
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
    use super::{export_file_name, SessionExportFormat};

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
}
