//! File-tree root and navigation operations.
//!
//! These GTK operations remain methods of the same Relm4 `AppModel` and keep the
//! existing file-tree store, header controller, and message routing unchanged.

use super::*;

impl AppModel {
    /// Rebuild the file tree with `root` at the top.
    #[allow(deprecated)]
    pub(crate) fn set_file_tree_root(&self, root: std::path::PathBuf) {
        let generation = self.file_tree_scan_generation.get().wrapping_add(1);
        self.file_tree_scan_generation.set(generation);
        self.file_tree_store.clear();
        self.file_header.emit(sidebar::FileHeaderMsg::SetRoot {
            display: file_tree::display_path(&root),
            tooltip: root.to_string_lossy().into_owned(),
        });
        *self.file_tree_root.borrow_mut() = root.clone();

        let store = self.file_tree_store.clone();
        let active_generation = self.file_tree_scan_generation.clone();
        let active_root = self.file_tree_root.clone();
        let expected_root = root.clone();
        if let Err(error) = file_tree::request_dir_scan(root, move |result| {
            if active_generation.get() != generation || *active_root.borrow() != expected_root {
                return;
            }
            match result {
                Ok(entries) => file_tree::append_entries(&store, None, entries),
                Err(error) => log::warn!(
                    "failed to scan file-tree root {}: {error}",
                    expected_root.display()
                ),
            }
        }) {
            log::warn!("failed to start file-tree scan: {error}");
        }
    }

    /// Initialize the file tree to the active cwd, else `$HOME`, else `/`.
    pub(crate) fn init_file_tree(&self) {
        let start = self
            .active_cwd()
            .or_else(file_tree::home_dir)
            .unwrap_or_else(|| std::path::PathBuf::from("/"));
        self.set_file_tree_root(start);
    }

    /// Jump the file tree to the active tab's working directory.
    pub(crate) fn file_tree_goto_current_cwd(&self) {
        match self.active_cwd() {
            Some(dir) => {
                if *self.file_tree_root.borrow() != dir {
                    self.set_file_tree_root(dir);
                }
            }
            None => {
                if self.file_tree_root.borrow().as_os_str().is_empty() {
                    if let Some(home) = file_tree::home_dir() {
                        self.set_file_tree_root(home);
                    }
                }
            }
        }
    }

    /// Move the file tree root up to its parent directory.
    pub(crate) fn file_tree_go_up(&self) {
        let parent = self
            .file_tree_root
            .borrow()
            .parent()
            .map(std::path::Path::to_path_buf);
        if let Some(parent) = parent {
            self.set_file_tree_root(parent);
        }
    }
}
