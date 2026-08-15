//! Sidebar file browser: a lazy-loading `TreeView` rooted at the active tab's
//! working directory (falling back to `$HOME`). Directories expand on demand;
//! activating a file inserts its shell-quoted path into the active terminal.
//! Ports forge's `ui/file_tree.rs` to anvil's relm4 structure.
//!
//! GTK4 deprecated the TreeView/TreeStore family in 4.10 in favor of the new
//! list/column views, but they remain fully functional and a ColumnView rewrite
//! is out of scope; suppress the deprecation lints module-wide.
#![allow(deprecated)]

use relm4::gtk;

use gtk::glib;
use gtk::prelude::*;
use gtk::{
    CellRendererPixbuf, CellRendererText, TreeIter, TreeRowReference, TreeStore, TreeView,
    TreeViewColumn,
};
use std::cell::Cell;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

// TreeStore column indices.
pub(crate) const COL_NAME: u32 = 0;
pub(crate) const COL_PATH: u32 = 1;
pub(crate) const COL_IS_DIR: u32 = 2;
pub(crate) const COL_ICON: u32 = 3;
pub(crate) const COL_TOOLTIP: u32 = 4;
const SCAN_POLL_INTERVAL: Duration = Duration::from_millis(16);
const MAX_DIRECTORY_ENTRIES: usize = 4_096;
const MAX_FILE_NAME_DISPLAY_BYTES: usize = 512;
const MAX_FILE_PATH_DISPLAY_BYTES: usize = 4 * 1024;
const MAX_FILE_PATH_IDENTITY_BYTES: usize = 64 * 1024;
const PATH_IDENTITY_PREFIX: &str = "unix-path-v1:";
const MAX_CONCURRENT_SCANS: usize = 16;
static ACTIVE_SCANS: AtomicUsize = AtomicUsize::new(0);

struct ScanPermit;

impl ScanPermit {
    fn acquire() -> io::Result<Self> {
        ACTIVE_SCANS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_CONCURRENT_SCANS).then_some(active + 1)
            })
            .map(|_| Self)
            .map_err(|_| io::Error::other("file-tree scan concurrency limit reached"))
    }
}

impl Drop for ScanPermit {
    fn drop(&mut self) {
        ACTIVE_SCANS.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Debug)]
pub(crate) struct FileEntry {
    name: String,
    path: PathBuf,
    is_dir: bool,
}

fn sort_entries(entries: &mut [FileEntry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

fn scan_dir(dir: &Path) -> io::Result<Vec<FileEntry>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)?
        .take(MAX_DIRECTORY_ENTRIES)
        .flatten()
    {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        entries.push(FileEntry {
            name: display_os_str(&entry.file_name()),
            // Do not follow directory symlinks: they can create cycles or turn
            // one expansion into a scan outside the tree the user selected.
            is_dir: file_type.is_dir(),
            path,
        });
    }
    sort_entries(&mut entries);
    Ok(entries)
}

/// Encode a Linux path for storage in GTK's string-only tree model without
/// ever treating its bytes as UTF-8. The versioned hex form is reversible and
/// explicitly bounded before its 2x expansion.
pub(crate) fn encode_path_identity(path: &Path) -> Option<String> {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let bytes = path.as_os_str().as_bytes();
    if bytes.len() > MAX_FILE_PATH_IDENTITY_BYTES {
        return None;
    }
    let encoded_len = PATH_IDENTITY_PREFIX
        .len()
        .checked_add(bytes.len().checked_mul(2)?)?;
    let mut encoded = String::with_capacity(encoded_len);
    encoded.push_str(PATH_IDENTITY_PREFIX);
    for &byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Some(encoded)
}

/// Recover the exact Linux path bytes stored by [`encode_path_identity`].
/// Malformed, unversioned, or oversized model values are rejected.
pub(crate) fn decode_path_identity(encoded: &str) -> Option<PathBuf> {
    let hex = encoded.strip_prefix(PATH_IDENTITY_PREFIX)?;
    if hex.len() % 2 != 0 || hex.len() / 2 > MAX_FILE_PATH_IDENTITY_BYTES {
        return None;
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for pair in hex.as_bytes().chunks_exact(2) {
        let high = hex_nibble(pair[0])?;
        let low = hex_nibble(pair[1])?;
        bytes.push((high << 4) | low);
    }
    Some(PathBuf::from(OsString::from_vec(bytes)))
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// Render valid UTF-8 normally and make every invalid byte visible. Escaping
/// literal backslashes keeps a real `\\xff` name distinct from a raw `0xff`.
pub(crate) fn display_os_str(value: &OsStr) -> String {
    let mut remaining = value.as_bytes();
    let mut display = String::with_capacity(remaining.len());
    while !remaining.is_empty() {
        match std::str::from_utf8(remaining) {
            Ok(valid) => {
                push_valid_display(&mut display, valid);
                break;
            }
            Err(error) => {
                let valid_len = error.valid_up_to();
                let valid = std::str::from_utf8(&remaining[..valid_len])
                    .expect("Utf8Error::valid_up_to must end on a UTF-8 boundary");
                push_valid_display(&mut display, valid);
                let invalid_len = error
                    .error_len()
                    .unwrap_or_else(|| remaining.len().saturating_sub(valid_len));
                for &byte in &remaining[valid_len..valid_len + invalid_len] {
                    use std::fmt::Write as _;
                    let _ = write!(display, "\\x{byte:02x}");
                }
                remaining = &remaining[valid_len + invalid_len..];
            }
        }
    }
    display
}

fn push_valid_display(display: &mut String, valid: &str) {
    for ch in valid.chars() {
        if ch == '\\' {
            display.push_str("\\\\");
        } else {
            display.push(ch);
        }
    }
}

pub(crate) fn is_notebook_path(path: &Path) -> bool {
    path.as_os_str().as_bytes().ends_with(b".jtnb.md")
}

pub(crate) fn request_dir_scan<F>(dir: PathBuf, apply: F) -> io::Result<()>
where
    F: FnOnce(io::Result<Vec<FileEntry>>) + 'static,
{
    let permit = ScanPermit::acquire()?;
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("anvil-file-tree-scan".to_string())
        .spawn(move || {
            let _permit = permit;
            let _ = tx.send(scan_dir(&dir));
        })?;

    let mut apply = Some(apply);
    glib::timeout_add_local(SCAN_POLL_INTERVAL, move || match rx.try_recv() {
        Ok(result) => {
            if let Some(apply) = apply.take() {
                apply(result);
            }
            glib::ControlFlow::Break
        }
        Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
        Err(mpsc::TryRecvError::Disconnected) => {
            if let Some(apply) = apply.take() {
                apply(Err(io::Error::other("file-tree scan worker disconnected")));
            }
            glib::ControlFlow::Break
        }
    });
    Ok(())
}

/// Display name, reversible path identity, is-directory, icon name, safe tooltip.
pub(crate) fn new_store() -> TreeStore {
    TreeStore::new(&[
        glib::Type::STRING,
        glib::Type::STRING,
        glib::Type::BOOL,
        glib::Type::STRING,
        glib::Type::STRING,
    ])
}

/// Build the headerless `TreeView` (icon + name in one column), no signals wired.
pub(crate) fn new_view(store: &TreeStore) -> TreeView {
    let view = TreeView::with_model(store);
    view.set_headers_visible(false);
    view.set_vexpand(true);
    view.set_tooltip_column(COL_TOOLTIP as i32);

    let column = TreeViewColumn::new();
    let icon = CellRendererPixbuf::new();
    column.pack_start(&icon, false);
    column.add_attribute(&icon, "icon-name", COL_ICON as i32);
    let text = CellRendererText::new();
    column.pack_start(&text, true);
    column.add_attribute(&text, "text", COL_NAME as i32);
    view.append_column(&column);
    view
}

/// Insert one row per pre-scanned directory entry under `parent`.
pub(crate) fn append_entries(
    store: &TreeStore,
    parent: Option<&TreeIter>,
    entries: Vec<FileEntry>,
) {
    for FileEntry { name, path, is_dir } in entries {
        let icon = if is_dir {
            "folder-symbolic"
        } else {
            "text-x-generic-symbolic"
        };
        let Some(path_identity) = encode_path_identity(&path) else {
            log::warn!(
                "file-tree path exceeds the {}-byte identity limit: {}",
                MAX_FILE_PATH_IDENTITY_BYTES,
                display_full_path(&path)
            );
            continue;
        };
        let display_name =
            crate::review_input::safe_inline_display(&name, MAX_FILE_NAME_DISPLAY_BYTES);
        let tooltip = display_full_path(&path);
        let iter = store.insert_with_values(
            parent,
            None,
            &[
                (COL_NAME, &display_name),
                (COL_PATH, &path_identity),
                (COL_IS_DIR, &is_dir),
                (COL_ICON, &icon),
                (COL_TOOLTIP, &tooltip),
            ],
        );
        if is_dir {
            // Placeholder child (empty path) → expander shows, loaded lazily.
            store.insert_with_values(
                Some(&iter),
                None,
                &[
                    (COL_NAME, &""),
                    (COL_PATH, &""),
                    (COL_IS_DIR, &false),
                    (COL_ICON, &""),
                    (COL_TOOLTIP, &""),
                ],
            );
        }
    }
}

/// Lazily fill a directory row's real children on first expansion.
pub(crate) fn on_expand(store: &TreeStore, iter: &TreeIter, scan_generation: &Rc<Cell<u64>>) {
    // A not-yet-loaded directory has a single placeholder child (empty path).
    let Some(first_child) = store.iter_children(Some(iter)) else {
        return;
    };
    let child_path: String = store
        .get_value(&first_child, COL_PATH as i32)
        .get()
        .unwrap_or_default();
    if !child_path.is_empty() {
        return; // already populated
    }
    let scan_in_progress: bool = store
        .get_value(&first_child, COL_IS_DIR as i32)
        .get()
        .unwrap_or(false);
    if scan_in_progress {
        return;
    }
    let dir_identity: String = store
        .get_value(iter, COL_PATH as i32)
        .get()
        .unwrap_or_default();
    if dir_identity.is_empty() {
        return;
    }
    let Some(dir_path) = decode_path_identity(&dir_identity) else {
        log::warn!("file-tree row contains an invalid path identity");
        return;
    };
    let Some(row_ref) = TreeRowReference::new(store, &store.path(iter)) else {
        return;
    };

    store.set(&first_child, &[(COL_IS_DIR, &true)]);
    let store_for_result = store.clone();
    let active_generation = scan_generation.clone();
    let generation = active_generation.get();
    let expected_identity = dir_identity.clone();
    let expected_display = display_full_path(&dir_path);
    if let Err(error) = request_dir_scan(dir_path, move |result| {
        if active_generation.get() != generation {
            return;
        }
        let Some(row_path) = row_ref.path() else {
            return;
        };
        let Some(parent) = store_for_result.iter(&row_path) else {
            return;
        };
        let current_path: String = store_for_result
            .get_value(&parent, COL_PATH as i32)
            .get()
            .unwrap_or_default();
        if current_path != expected_identity {
            return;
        }
        let Some(placeholder) = store_for_result.iter_children(Some(&parent)) else {
            return;
        };
        let placeholder_path: String = store_for_result
            .get_value(&placeholder, COL_PATH as i32)
            .get()
            .unwrap_or_default();
        if !placeholder_path.is_empty() {
            return;
        }
        store_for_result.remove(&placeholder);
        match result {
            Ok(entries) => append_entries(&store_for_result, Some(&parent), entries),
            Err(error) => log::warn!("failed to scan directory {expected_display}: {error}"),
        }
    }) {
        store.set(&first_child, &[(COL_IS_DIR, &false)]);
        log::warn!("failed to start directory scan: {error}");
    }
}

pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Abbreviate the home directory to `~` for the header label.
pub(crate) fn display_path(path: &Path) -> String {
    let display = if let Some(home) = home_dir() {
        if let Ok(rel) = path.strip_prefix(&home) {
            if rel.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", display_os_str(rel.as_os_str()))
            }
        } else {
            display_os_str(path.as_os_str())
        }
    } else {
        display_os_str(path.as_os_str())
    };
    crate::review_input::safe_inline_display(&display, MAX_FILE_PATH_DISPLAY_BYTES)
}

pub(crate) fn display_full_path(path: &Path) -> String {
    crate::review_input::safe_inline_display(
        &display_os_str(path.as_os_str()),
        MAX_FILE_PATH_DISPLAY_BYTES,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn entries_sort_directories_first_then_by_name() {
        let mut entries = vec![
            FileEntry {
                name: "Zulu.txt".into(),
                path: PathBuf::from("Zulu.txt"),
                is_dir: false,
            },
            FileEntry {
                name: "beta".into(),
                path: PathBuf::from("beta"),
                is_dir: true,
            },
            FileEntry {
                name: "Alpha.txt".into(),
                path: PathBuf::from("Alpha.txt"),
                is_dir: false,
            },
            FileEntry {
                name: "Able".into(),
                path: PathBuf::from("Able"),
                is_dir: true,
            },
        ];

        sort_entries(&mut entries);

        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["Able", "beta", "Alpha.txt", "Zulu.txt"]);
    }

    #[test]
    fn non_utf8_path_identities_round_trip_without_colliding() {
        let ff = PathBuf::from(OsString::from_vec(b"a\xff".to_vec()));
        let fe = PathBuf::from(OsString::from_vec(b"a\xfe".to_vec()));

        let ff_identity = encode_path_identity(&ff).expect("bounded path should encode");
        let fe_identity = encode_path_identity(&fe).expect("bounded path should encode");

        assert_ne!(ff_identity, fe_identity);
        assert_eq!(decode_path_identity(&ff_identity), Some(ff.clone()));
        assert_eq!(decode_path_identity(&fe_identity), Some(fe.clone()));
        assert_eq!(display_os_str(ff.as_os_str()), r"a\xff");
        assert_eq!(display_os_str(fe.as_os_str()), r"a\xfe");
    }

    #[test]
    fn path_identity_rejects_malformed_or_oversized_values() {
        assert_eq!(decode_path_identity(""), None);
        assert_eq!(decode_path_identity("unix-path-v1:0"), None);
        assert_eq!(decode_path_identity("unix-path-v1:gg"), None);
        let oversized = PathBuf::from(OsString::from_vec(vec![
            b'a';
            MAX_FILE_PATH_IDENTITY_BYTES + 1
        ]));
        assert_eq!(encode_path_identity(&oversized), None);
    }
}
