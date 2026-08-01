//! Sidebar file browser: a lazy-loading `TreeView` rooted at the active tab's
//! working directory (falling back to `$HOME`). Directories expand on demand;
//! activating a file inserts its shell-quoted path into the active terminal.
//! Ports jterm4's `ui/file_tree.rs` to jterm1's relm4 structure.
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
use std::io;
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
            name: entry.file_name().to_string_lossy().into_owned(),
            // Do not follow directory symlinks: they can create cycles or turn
            // one expansion into a scan outside the tree the user selected.
            is_dir: file_type.is_dir(),
            path,
        });
    }
    sort_entries(&mut entries);
    Ok(entries)
}

pub(crate) fn request_dir_scan<F>(dir: PathBuf, apply: F) -> io::Result<()>
where
    F: FnOnce(io::Result<Vec<FileEntry>>) + 'static,
{
    let permit = ScanPermit::acquire()?;
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("jterm1-file-tree-scan".to_string())
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

/// Display name, raw absolute path, is-directory, icon name, safe tooltip.
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
        let path_str = path.to_string_lossy().to_string();
        let display_name =
            crate::text_safety::bounded_display_text(&name, MAX_FILE_NAME_DISPLAY_BYTES, false);
        let tooltip =
            crate::text_safety::bounded_display_text(&path_str, MAX_FILE_PATH_DISPLAY_BYTES, false);
        let iter = store.insert_with_values(
            parent,
            None,
            &[
                (COL_NAME, &display_name),
                (COL_PATH, &path_str),
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
    let dir_path: String = store
        .get_value(iter, COL_PATH as i32)
        .get()
        .unwrap_or_default();
    if dir_path.is_empty() {
        return;
    }
    let Some(row_ref) = TreeRowReference::new(store, &store.path(iter)) else {
        return;
    };

    store.set(&first_child, &[(COL_IS_DIR, &true)]);
    let store_for_result = store.clone();
    let active_generation = scan_generation.clone();
    let generation = active_generation.get();
    let expected_path = dir_path.clone();
    if let Err(error) = request_dir_scan(PathBuf::from(dir_path), move |result| {
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
        if current_path != expected_path {
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
            Err(error) => log::warn!("failed to scan directory {expected_path}: {error}"),
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
                format!("~/{}", rel.to_string_lossy())
            }
        } else {
            path.to_string_lossy().to_string()
        }
    } else {
        path.to_string_lossy().to_string()
    };
    crate::text_safety::bounded_display_text(&display, MAX_FILE_PATH_DISPLAY_BYTES, false)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
