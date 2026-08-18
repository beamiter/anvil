//! File-tree root, navigation, location, and file-operation handlers.
//!
//! These GTK operations remain methods of the same Relm4 `AppModel` and keep the
//! existing file-tree store, header controller, and message routing unchanged.
//! Blocking filesystem work — local disk or the remote probe — always runs on
//! worker threads behind `file_tree`'s thread + mpsc + glib-poll skeleton.

use super::*;

impl AppModel {
    /// Rebuild the file tree with `root` at the top of the current location.
    #[allow(deprecated)]
    pub(crate) fn set_file_tree_root(&self, root: std::path::PathBuf) {
        let generation = self.file_tree_scan_generation.get().wrapping_add(1);
        self.file_tree_scan_generation.set(generation);
        self.file_tree_store.clear();
        let loc = self.file_tree_location.borrow().clone();
        // `~` abbreviation uses the LOCAL home; it would lie about a remote
        // path that happens to sit under the same prefix.
        let display = if loc == remote_fs::FsLocation::Local {
            file_tree::display_path(&root)
        } else {
            file_tree::display_full_path(&root)
        };
        self.file_header.emit(sidebar::FileHeaderMsg::SetRoot {
            display,
            tooltip: file_tree::display_full_path(&root),
        });
        *self.file_tree_root.borrow_mut() = root.clone();

        let hosts = self.config.borrow().remote_hosts.clone();
        let store = self.file_tree_store.clone();
        let active_generation = self.file_tree_scan_generation.clone();
        let active_root = self.file_tree_root.clone();
        let active_location = self.file_tree_location.clone();
        let expected_loc = loc.clone();
        let expected_root = root.clone();
        if let Err(error) = file_tree::request_dir_scan(loc, hosts, root, move |result| {
            if active_generation.get() != generation
                || *active_root.borrow() != expected_root
                || *active_location.borrow() != expected_loc
            {
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

    /// Jump the file tree to the active tab's working directory. A remote tab
    /// reports its cwd in the ssh session's own namespace, so the tree follows
    /// by browsing the matching configured host there; a local cwd only drives
    /// the Local location and never yanks a deliberately browsed remote tree.
    pub(crate) fn file_tree_goto_current_cwd(&self) {
        if let Some((loc, cwd)) = self.active_remote_cwd() {
            if *self.file_tree_location.borrow() != loc {
                *self.file_tree_location.borrow_mut() = loc;
                self.sync_file_header_locations();
            }
            if *self.file_tree_root.borrow() != cwd {
                self.set_file_tree_root(cwd);
            }
            return;
        }
        if *self.file_tree_location.borrow() != remote_fs::FsLocation::Local {
            return;
        }
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

    /// The active pane's remote cwd paired with its configured host location,
    /// if the active tab is a remote session whose host is still configured.
    /// Only absolute paths are followed: a garbled report must not turn into
    /// a probe that the Rust-side validation would reject anyway.
    fn active_remote_cwd(&self) -> Option<(remote_fs::FsLocation, std::path::PathBuf)> {
        let tab = self.tabs.get(self.active)?;
        let conn = tab.remote.as_ref()?;
        let pane = tab.panes.get(tab.active_pane)?;
        if !pane.cwd_external {
            return None;
        }
        let cwd = pane.cwd.as_deref()?;
        if !std::path::Path::new(cwd).is_absolute() {
            return None;
        }
        let hosts = &self.config.borrow().remote_hosts;
        let index = hosts.iter().position(|host| host.name == conn.host.name)?;
        Some((
            remote_fs::FsLocation::Remote(index),
            std::path::PathBuf::from(cwd),
        ))
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

    /// Reload the current root: bump the generation and scan it again.
    pub(crate) fn file_tree_refresh(&self) {
        let root = self.file_tree_root.borrow().clone();
        if root.as_os_str().is_empty() {
            self.init_file_tree();
        } else {
            self.set_file_tree_root(root);
        }
    }

    /// Selector moved: resolve the new location's start directory off-thread,
    /// then let `FileTreeLocationResolved` re-root the tree if the location
    /// is still current by the time the probe answers.
    #[allow(deprecated)]
    pub(crate) fn file_tree_select_location(
        &self,
        index: usize,
        sender: &ComponentSender<AppModel>,
    ) {
        let hosts = self.config.borrow().remote_hosts.clone();
        let loc = if index == 0 {
            remote_fs::FsLocation::Local
        } else if index <= hosts.len() {
            remote_fs::FsLocation::Remote(index - 1)
        } else {
            return; // stale dropdown after a config edit
        };
        if *self.file_tree_location.borrow() == loc {
            return;
        }
        *self.file_tree_location.borrow_mut() = loc.clone();

        // Clear immediately so rows from the old location cannot be acted on
        // while the new start directory resolves over ssh/docker.
        let generation = self.file_tree_scan_generation.get().wrapping_add(1);
        self.file_tree_scan_generation.set(generation);
        self.file_tree_store.clear();
        self.file_header.emit(sidebar::FileHeaderMsg::SetRoot {
            display: loc.label(&hosts),
            tooltip: String::new(),
        });

        let active_location = self.file_tree_location.clone();
        let start_loc = loc.clone();
        let check_loc = loc.clone();
        let sender = sender.clone();
        if let Err(error) = file_tree::request_fs_op(
            move || remote_fs::start_dir(&start_loc, &hosts),
            move |result| {
                if *active_location.borrow() == check_loc {
                    sender.input(AppMsg::FileTreeLocationResolved {
                        loc: check_loc,
                        start: result.map_err(|error| error.to_string()),
                    });
                }
            },
        ) {
            log::warn!("failed to start remote home probe: {error}");
            self.file_tree_location_resolved(loc, Err(error.to_string()));
        }
    }

    /// Finish a location switch on the GTK thread.
    pub(crate) fn file_tree_location_resolved(
        &self,
        loc: remote_fs::FsLocation,
        start: Result<std::path::PathBuf, String>,
    ) {
        if *self.file_tree_location.borrow() != loc {
            return;
        }
        match start {
            Ok(root) => self.set_file_tree_root(root),
            Err(error) => {
                // A host that cannot answer `home` cannot list either; roll
                // back to Local rather than strand the tree on a dead host.
                *self.file_tree_location.borrow_mut() = remote_fs::FsLocation::Local;
                self.sync_file_header_locations();
                let label = loc.label(&self.config.borrow().remote_hosts);
                self.show_toast(format!(
                    "Cannot browse {label}: {}",
                    review_input::safe_inline_display(&error, 512)
                ));
                let root = file_tree::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));
                self.set_file_tree_root(root);
            }
        }
    }

    /// Push the current labels + selection into the header's location
    /// selector after a config change or a rollback to Local.
    pub(crate) fn sync_file_header_locations(&self) {
        let hosts = self.config.borrow().remote_hosts.clone();
        let selected = match &*self.file_tree_location.borrow() {
            remote_fs::FsLocation::Local => 0,
            remote_fs::FsLocation::Remote(index) => (index + 1).min(hosts.len()),
        };
        self.file_header.emit(sidebar::FileHeaderMsg::SetLocations {
            labels: remote_fs::location_labels(&hosts),
            selected,
        });
    }

    /// Ask for a name, then create the entry on confirm. `dir: None` targets
    /// the current tree root.
    pub(crate) fn file_tree_prompt_new(
        &self,
        dir: Option<std::path::PathBuf>,
        is_dir: bool,
        sender: &ComponentSender<AppModel>,
    ) {
        let dir = dir.unwrap_or_else(|| self.file_tree_root.borrow().clone());
        if dir.as_os_str().is_empty() {
            return;
        }
        let title = if is_dir { "New Folder" } else { "New File" };
        self.prompt_file_name(title, "Create", None, sender, move |name| {
            AppMsg::FileTreeCreateNamed {
                dir: dir.clone(),
                name,
                is_dir,
            }
        });
    }

    /// Ask for the new name of `path`, then rename on confirm. A non-UTF-8
    /// name is NOT prefilled: the entry would edit its `\xff` display escapes
    /// and rename the file to the literal escape text.
    pub(crate) fn file_tree_prompt_rename(
        &self,
        path: std::path::PathBuf,
        sender: &ComponentSender<AppModel>,
    ) {
        let initial = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string);
        self.prompt_file_name(
            "Rename",
            "Rename",
            initial.as_deref(),
            sender,
            move |name| AppMsg::FileTreeRenameNamed {
                src: path.clone(),
                name,
            },
        );
    }

    /// Destructive ops name their target and wait for an explicit confirm.
    pub(crate) fn file_tree_confirm_delete(
        &self,
        path: std::path::PathBuf,
        sender: &ComponentSender<AppModel>,
    ) {
        let body = format!(
            "Delete {} permanently?\n\nThis cannot be undone.",
            file_tree::display_full_path(&path)
        );
        let dialog = adw::AlertDialog::new(Some("Delete"), Some(&body));
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        {
            let sender = sender.clone();
            dialog.connect_response(None, move |_, response| {
                if response == "delete" {
                    sender.input(AppMsg::FileTreeDeleteConfirmed(path.clone()));
                }
            });
        }
        dialog.present(Some(&self.window));
    }

    /// Remember one Copy/Cut row, tagged with the location it came from.
    pub(crate) fn file_tree_clipboard_set(
        &self,
        path: std::path::PathBuf,
        is_dir: bool,
        cut: bool,
    ) {
        *self.file_tree_clipboard.borrow_mut() = Some(remote_fs::FsClipboard {
            loc: self.file_tree_location.borrow().clone(),
            path,
            is_dir,
            cut,
        });
    }

    pub(crate) fn file_tree_create_named(
        &self,
        dir: std::path::PathBuf,
        name: String,
        is_dir: bool,
        sender: &ComponentSender<AppModel>,
    ) {
        if let Some(error) = remote_fs::new_name_error(&name) {
            self.show_toast(format!("Invalid name: {error}"));
            return;
        }
        let path = dir.join(&name);
        let loc = self.file_tree_location.borrow().clone();
        let hosts = self.config.borrow().remote_hosts.clone();
        self.run_file_tree_op(
            None,
            move || {
                if is_dir {
                    remote_fs::create_dir(&loc, &hosts, &path)
                } else {
                    remote_fs::create_file(&loc, &hosts, &path)
                }
            },
            sender,
        );
    }

    pub(crate) fn file_tree_rename_named(
        &self,
        src: std::path::PathBuf,
        name: String,
        sender: &ComponentSender<AppModel>,
    ) {
        if let Some(error) = remote_fs::new_name_error(&name) {
            self.show_toast(format!("Invalid name: {error}"));
            return;
        }
        let Some(parent) = src.parent() else {
            self.show_toast("The filesystem root cannot be renamed.");
            return;
        };
        let dst = parent.join(&name);
        if dst == src {
            return;
        }
        let loc = self.file_tree_location.borrow().clone();
        let hosts = self.config.borrow().remote_hosts.clone();
        // A renamed clipboard source would dangle; forget it on success.
        self.run_file_tree_op(
            Some(src.clone()),
            move || remote_fs::rename(&loc, &hosts, &src, &dst),
            sender,
        );
    }

    pub(crate) fn file_tree_delete_confirmed(
        &self,
        path: std::path::PathBuf,
        sender: &ComponentSender<AppModel>,
    ) {
        let loc = self.file_tree_location.borrow().clone();
        let hosts = self.config.borrow().remote_hosts.clone();
        // A deleted clipboard source would dangle; forget it on success.
        self.run_file_tree_op(
            Some(path.clone()),
            move || remote_fs::delete(&loc, &hosts, &path),
            sender,
        );
    }

    /// Paste the clipboard into `dir` (the root when None): cut becomes a
    /// rename, copy a recursive copy. Cross-location paste is refused.
    pub(crate) fn file_tree_paste(
        &self,
        dir: Option<std::path::PathBuf>,
        sender: &ComponentSender<AppModel>,
    ) {
        let Some(clip) = self.file_tree_clipboard.borrow().clone() else {
            return;
        };
        let loc = self.file_tree_location.borrow().clone();
        if clip.loc != loc {
            // The menu keeps Paste insensitive in this state; this catches a
            // clipboard or location that changed while the menu was open.
            self.show_toast("Paste stays within one browsing location.");
            return;
        }
        let dir = dir.unwrap_or_else(|| self.file_tree_root.borrow().clone());
        if dir.as_os_str().is_empty() {
            return;
        }
        let dst = remote_fs::paste_destination(&dir, &clip.path);
        if dst == clip.path {
            self.show_toast("Copy and cut need a different target directory.");
            return;
        }
        let hosts = self.config.borrow().remote_hosts.clone();
        let src = clip.path.clone();
        let cut = clip.cut;
        // A successful cut-paste consumes the clipboard.
        self.run_file_tree_op(
            cut.then_some(src.clone()),
            move || {
                if cut {
                    remote_fs::rename(&loc, &hosts, &src, &dst)
                } else {
                    remote_fs::copy(&loc, &hosts, &src, &dst)
                }
            },
            sender,
        );
    }

    /// Shared name-entry dialog for New File / New Folder / Rename. Invalid
    /// names are rejected before the follow-up message is sent.
    fn prompt_file_name(
        &self,
        title: &str,
        action: &str,
        initial: Option<&str>,
        sender: &ComponentSender<AppModel>,
        on_name: impl Fn(String) -> AppMsg + 'static,
    ) {
        let dialog = adw::AlertDialog::new(Some(title), None);
        let entry = gtk::Entry::new();
        entry.set_activates_default(true);
        entry.set_max_length(255);
        if let Some(initial) = initial {
            entry.set_text(initial);
            entry.select_region(0, -1);
        }
        dialog.set_extra_child(Some(&entry));
        dialog.add_responses(&[("cancel", "Cancel"), ("ok", action)]);
        dialog.set_default_response(Some("ok"));
        dialog.set_close_response("cancel");
        {
            let sender = sender.clone();
            dialog.connect_response(None, move |_, response| {
                if response != "ok" {
                    return;
                }
                let name = entry.text().to_string();
                if let Some(error) = remote_fs::new_name_error(&name) {
                    sender.input(AppMsg::Toast(format!("Invalid name: {error}")));
                    return;
                }
                sender.input(on_name(name));
            });
        }
        dialog.present(Some(&self.window));
    }

    /// Run one blocking op on a worker thread, then reload the tree on the
    /// GTK thread. `forget_clipboard` names a clipboard source that a
    /// successful rename/delete/cut-paste consumes or makes dangle.
    fn run_file_tree_op(
        &self,
        forget_clipboard: Option<std::path::PathBuf>,
        op: impl FnOnce() -> std::io::Result<()> + Send + 'static,
        sender: &ComponentSender<AppModel>,
    ) {
        let toast_overlay = self.toast_overlay.clone();
        let clipboard = self.file_tree_clipboard.clone();
        let sender = sender.clone();
        if let Err(error) = file_tree::request_fs_op(op, move |result| match result {
            Ok(()) => {
                if let Some(path) = &forget_clipboard {
                    let dangling = clipboard
                        .borrow()
                        .as_ref()
                        .is_some_and(|clip| &clip.path == path);
                    if dangling {
                        *clipboard.borrow_mut() = None;
                    }
                }
                sender.input(AppMsg::FileTreeRefresh);
            }
            Err(error) => {
                let message = review_input::safe_inline_display(&error.to_string(), 512);
                toast_overlay.add_toast(adw::Toast::new(&format!(
                    "File operation failed: {message}"
                )));
            }
        }) {
            self.show_toast(format!("Could not start the file operation: {error}"));
        }
    }
}
