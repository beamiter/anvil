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
use std::sync::mpsc;
use std::time::Duration;

// TreeStore column indices.
pub(crate) const COL_NAME: u32 = 0;
pub(crate) const COL_PATH: u32 = 1;
pub(crate) const COL_IS_DIR: u32 = 2;
pub(crate) const COL_ICON: u32 = 3;
const SCAN_POLL_INTERVAL: Duration = Duration::from_millis(16);

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
    for entry in std::fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        entries.push(FileEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            is_dir: path.is_dir(),
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
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("jterm1-file-tree-scan".to_string())
        .spawn(move || {
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

/// A four-column store: display name, absolute path, is-directory, icon name.
pub(crate) fn new_store() -> TreeStore {
    TreeStore::new(&[
        glib::Type::STRING,
        glib::Type::STRING,
        glib::Type::BOOL,
        glib::Type::STRING,
    ])
}

/// Build the headerless `TreeView` (icon + name in one column), no signals wired.
pub(crate) fn new_view(store: &TreeStore) -> TreeView {
    let view = TreeView::with_model(store);
    view.set_headers_visible(false);
    view.set_vexpand(true);

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
        let iter = store.insert_with_values(
            parent,
            None,
            &[
                (COL_NAME, &name),
                (COL_PATH, &path_str),
                (COL_IS_DIR, &is_dir),
                (COL_ICON, &icon),
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
    if let Some(home) = home_dir() {
        if let Ok(rel) = path.strip_prefix(&home) {
            if rel.as_os_str().is_empty() {
                return "~".to_string();
            }
            return format!("~/{}", rel.to_string_lossy());
        }
    }
    path.to_string_lossy().to_string()
}

/// Single-quote a path for safe shell insertion.
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
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
