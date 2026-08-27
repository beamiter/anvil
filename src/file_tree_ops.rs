//! File-tree root, navigation, location, and file-operation handlers.
//!
//! These GTK operations remain methods of the same Relm4 `AppModel` and keep the
//! existing file-tree store, header controller, and message routing unchanged.
//! Blocking filesystem work — local disk or the remote probe — always runs on
//! worker threads behind `file_tree`'s thread + mpsc + glib-poll skeleton.

use super::*;

impl AppModel {
    /// Rebuild the file tree with `root` at the top of the current location.
    /// An open filter is closed: the fresh rows would otherwise be invisible
    /// until the query is retyped.
    #[allow(deprecated)]
    pub(crate) fn set_file_tree_root(&self, root: std::path::PathBuf) {
        self.file_header.emit(sidebar::FileHeaderMsg::CloseFilter);
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
            let (location_changed, reroot) = {
                let current_location = self.file_tree_location.borrow();
                let current_root = self.file_tree_root.borrow();
                (
                    *current_location != loc,
                    file_tree::file_tree_follow_requires_reroot(
                        &current_location,
                        &loc,
                        &current_root,
                        &cwd,
                    ),
                )
            };
            if location_changed {
                *self.file_tree_location.borrow_mut() = loc.clone();
                self.sync_file_header_locations();
            }
            if reroot {
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
        if pane.id != conn.pane_id || !pane.cwd_external {
            return None;
        }
        let cwd = pane.cwd.as_deref()?;
        if !std::path::Path::new(cwd).is_absolute() {
            return None;
        }
        let config = self.config.borrow();
        let index = config::unique_checked_remote_profile_index(
            &config.remote_hosts,
            conn.configured_profile(),
        )?;
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
    pub(crate) fn file_tree_refresh(&self, intent: file_tree::FileTreeIntent) {
        if !self.require_current_file_tree_intent(&intent) {
            return;
        }
        let root = self.file_tree_root.borrow().clone();
        if root.as_os_str().is_empty() {
            self.init_file_tree();
        } else {
            self.set_file_tree_root(root);
        }
    }

    /// Open a terminal from the file-tree header without confusing a remote
    /// browsing path for a remote-shell cwd. Local launches use the exact tree
    /// root; remote launches revalidate and clone only the selected managed
    /// profile, then use the ordinary connection lifecycle.
    pub(crate) fn file_tree_open_terminal(
        &mut self,
        intent: file_tree::FileTreeIntent,
        sender: &ComponentSender<AppModel>,
    ) {
        if !self.require_current_file_tree_intent(&intent) {
            return;
        }
        let location = self.file_tree_location.borrow().clone();
        let root = self.file_tree_root.borrow().clone();
        let target = {
            let config = self.config.borrow();
            file_tree::terminal_target(&location, &root, &config.remote_hosts)
        };
        match target {
            Ok(file_tree::FileTreeTerminalTarget::Local(cwd)) => {
                let startup = self.config.borrow().startup_commands.clone();
                self.add_tab_with(
                    InitialCommands::from_config(startup.as_deref()),
                    Some(cwd),
                    self.shell_argv.clone(),
                    sender,
                );
            }
            Ok(file_tree::FileTreeTerminalTarget::Remote(host)) => {
                if self.safe_mode {
                    self.show_toast("Remote connections are disabled in safe mode.");
                } else {
                    self.add_remote_tab(&host, sender);
                }
            }
            Err(message) => self.show_toast(format!("Cannot open terminal: {message}")),
        }
    }

    /// Apply the header filter entry's query to the loaded tree rows.
    #[allow(deprecated)]
    pub(crate) fn file_tree_apply_filter(&self, query: &str) {
        let mut state = self.file_tree_filter.borrow_mut();
        file_tree::apply_tree_filter(
            &self.file_tree_store,
            &self.file_tree_view,
            &self.file_tree_filter_model,
            &mut state,
            query,
        );
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
        } else if index <= hosts.len() && config::checked_remote_host(&hosts, index - 1).is_ok() {
            remote_fs::FsLocation::Remote(index - 1)
        } else {
            return; // stale dropdown after a config edit
        };
        if *self.file_tree_location.borrow() == loc {
            return;
        }

        self.begin_file_tree_location_switch(loc, hosts, sender);
    }

    /// Start resolving one selected filesystem location. Clearing both the
    /// visible rows and the old root makes pending state explicit and prevents
    /// a later profile remap from probing a local/other-host path remotely.
    #[allow(deprecated)]
    fn begin_file_tree_location_switch(
        &self,
        loc: remote_fs::FsLocation,
        hosts: Vec<config::RemoteHost>,
        sender: &ComponentSender<AppModel>,
    ) {
        *self.file_tree_location.borrow_mut() = loc.clone();

        // Clear immediately so rows from the old location cannot be acted on
        // while the new start directory resolves over ssh/docker.
        let generation = self.file_tree_scan_generation.get().wrapping_add(1);
        self.file_tree_scan_generation.set(generation);
        self.file_tree_store.clear();
        self.file_tree_root.borrow_mut().clear();
        self.file_header.emit(sidebar::FileHeaderMsg::SetRoot {
            display: loc.label(&hosts),
            tooltip: String::new(),
        });

        let start_loc = loc.clone();
        let intent = file_tree::capture_file_tree_intent(generation, &loc, &hosts);
        let callback_intent = intent.clone();
        let active_generation = self.file_tree_scan_generation.clone();
        let active_location = self.file_tree_location.clone();
        let active_config = self.config.clone();
        let sender = sender.clone();
        if let Err(error) = file_tree::request_fs_op(
            move || remote_fs::start_dir(&start_loc, &hosts),
            move |result| {
                let still_current = {
                    let location = active_location.borrow();
                    let config = active_config.borrow();
                    file_tree::file_tree_intent_is_current(
                        &callback_intent,
                        active_generation.get(),
                        &location,
                        &config.remote_hosts,
                    )
                };
                if still_current {
                    sender.input(AppMsg::FileTreeLocationResolved {
                        intent: Box::new(callback_intent),
                        start: result.map_err(|error| error.to_string()),
                    });
                }
            },
        ) {
            log::warn!("failed to start remote home probe: {error}");
            self.file_tree_location_resolved(intent, Err(error.to_string()));
        }
    }

    /// Rebind an index-backed tree location after `remote_hosts` changes.
    /// Exactly one complete-profile match may survive. Reordering restarts any
    /// in-flight work against the remapped index; missing, edited, invalid, or
    /// ambiguous identities return to Local and synchronously clear old rows.
    pub(crate) fn reconcile_file_tree_remote_hosts(
        &self,
        old_hosts: &[config::RemoteHost],
        sender: &ComponentSender<AppModel>,
    ) {
        let previous = self.file_tree_location.borrow().clone();
        let new_hosts = self.config.borrow().remote_hosts.clone();
        let remapped = remote_fs::remap_location_by_profile(&previous, old_hosts, &new_hosts);
        remote_fs::remap_clipboard_by_profile(
            &mut self.file_tree_clipboard.borrow_mut(),
            old_hosts,
            &new_hosts,
        );

        if remapped != previous {
            match remapped.clone() {
                remote_fs::FsLocation::Local => {
                    *self.file_tree_location.borrow_mut() = remote_fs::FsLocation::Local;
                    let root =
                        file_tree::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));
                    self.set_file_tree_root(root);
                }
                remote_fs::FsLocation::Remote(_) => {
                    let root = self.file_tree_root.borrow().clone();
                    if root.as_os_str().is_empty() {
                        self.begin_file_tree_location_switch(remapped, new_hosts.clone(), sender);
                    } else {
                        *self.file_tree_location.borrow_mut() = remapped;
                        self.set_file_tree_root(root);
                    }
                }
            }
        }
        self.sync_file_header_locations();
    }

    /// Finish a location switch on the GTK thread.
    pub(crate) fn file_tree_location_resolved(
        &self,
        intent: file_tree::FileTreeIntent,
        start: Result<std::path::PathBuf, String>,
    ) {
        // Recheck after message delivery as well as before enqueueing it: the
        // GTK queue itself is an async boundary across which location/config
        // can change again.
        if !self.file_tree_intent_is_current(&intent) {
            return;
        }
        let loc = self.file_tree_location.borrow().clone();
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

    fn file_tree_intent_is_current(&self, intent: &file_tree::FileTreeIntent) -> bool {
        let location = self.file_tree_location.borrow();
        let config = self.config.borrow();
        file_tree::file_tree_intent_is_current(
            intent,
            self.file_tree_scan_generation.get(),
            &location,
            &config.remote_hosts,
        )
    }

    /// A stale dialog fails closed before name/path validation or any worker
    /// operation can reinterpret its targets against another filesystem.
    fn require_current_file_tree_intent(&self, intent: &file_tree::FileTreeIntent) -> bool {
        let current = self.file_tree_intent_is_current(intent);
        if !current {
            self.show_toast("The file-tree location changed; the pending operation was cancelled.");
        }
        current
    }

    /// Ask for a name, then create the entry on confirm. `dir: None` targets
    /// the current tree root.
    pub(crate) fn file_tree_prompt_new(
        &self,
        dir: Option<std::path::PathBuf>,
        is_dir: bool,
        intent: file_tree::FileTreeIntent,
        sender: &ComponentSender<AppModel>,
    ) {
        if !self.require_current_file_tree_intent(&intent) {
            return;
        }
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
                intent: Box::new(intent.clone()),
            }
        });
    }

    /// Ask for the new name of `path`, then rename on confirm. A non-UTF-8
    /// name is NOT prefilled: the entry would edit its `\xff` display escapes
    /// and rename the file to the literal escape text.
    pub(crate) fn file_tree_prompt_rename(
        &self,
        path: std::path::PathBuf,
        intent: file_tree::FileTreeIntent,
        sender: &ComponentSender<AppModel>,
    ) {
        if !self.require_current_file_tree_intent(&intent) {
            return;
        }
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
                intent: Box::new(intent.clone()),
            },
        );
    }

    /// Destructive ops name their target and wait for an explicit confirm;
    /// a batch delete lists the count and up to five names.
    pub(crate) fn file_tree_confirm_delete(
        &self,
        paths: Vec<std::path::PathBuf>,
        intent: file_tree::FileTreeIntent,
        sender: &ComponentSender<AppModel>,
    ) {
        if paths.is_empty() {
            return;
        }
        if !self.require_current_file_tree_intent(&intent) {
            return;
        }
        let body = if paths.len() == 1 {
            format!(
                "Delete {} permanently?\n\nThis cannot be undone.",
                file_tree::display_full_path(&paths[0])
            )
        } else {
            let mut body = format!("Delete {} items permanently?\n\n", paths.len());
            for path in paths.iter().take(5) {
                body.push_str(&file_tree::display_full_path(path));
                body.push('\n');
            }
            if paths.len() > 5 {
                body.push_str(&format!("…and {} more\n", paths.len() - 5));
            }
            body.push_str("\nThis cannot be undone.");
            body
        };
        let dialog = adw::AlertDialog::new(Some("Delete"), Some(&body));
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        {
            let sender = sender.clone();
            dialog.connect_response(None, move |_, response| {
                if response == "delete" {
                    sender.input(AppMsg::FileTreeDeleteConfirmed {
                        paths: paths.clone(),
                        intent: Box::new(intent.clone()),
                    });
                }
            });
        }
        dialog.present(Some(&self.window));
    }

    /// Remember Copy/Cut rows, tagged with the location they came from.
    pub(crate) fn file_tree_clipboard_set(
        &self,
        items: Vec<(std::path::PathBuf, bool)>,
        cut: bool,
        intent: file_tree::FileTreeIntent,
    ) {
        if items.is_empty() {
            return;
        }
        if !self.require_current_file_tree_intent(&intent) {
            return;
        }
        let Some(token) = self.file_tree_clipboard_revision.get().checked_add(1) else {
            self.show_toast("The file clipboard identity space is exhausted; restart Anvil.");
            return;
        };
        self.file_tree_clipboard_revision.set(token);
        *self.file_tree_clipboard.borrow_mut() = Some(remote_fs::FsClipboard {
            loc: self.file_tree_location.borrow().clone(),
            items: items
                .into_iter()
                .map(|(path, is_dir)| remote_fs::FsClipboardItem { path, is_dir })
                .collect(),
            cut,
            token,
        });
    }

    pub(crate) fn file_tree_create_named(
        &self,
        dir: std::path::PathBuf,
        name: String,
        is_dir: bool,
        intent: file_tree::FileTreeIntent,
        sender: &ComponentSender<AppModel>,
    ) {
        if !self.require_current_file_tree_intent(&intent) {
            return;
        }
        if let Some(error) = remote_fs::new_name_error(&name) {
            self.show_toast(format!("Invalid name: {error}"));
            return;
        }
        let path = dir.join(&name);
        let loc = self.file_tree_location.borrow().clone();
        let hosts = self.config.borrow().remote_hosts.clone();
        self.run_file_tree_op(
            intent,
            None,
            Vec::new(),
            vec![dir],
            move || {
                if is_dir {
                    remote_fs::create_dir(&loc, &hosts, &path)
                } else {
                    remote_fs::create_file(&loc, &hosts, &path)
                }
            },
            |_| {},
            |_| {},
            sender,
        );
    }

    pub(crate) fn file_tree_rename_named(
        &self,
        src: std::path::PathBuf,
        name: String,
        intent: file_tree::FileTreeIntent,
        sender: &ComponentSender<AppModel>,
    ) {
        if !self.require_current_file_tree_intent(&intent) {
            return;
        }
        if let Some(error) = remote_fs::new_name_error(&name) {
            self.show_toast(format!("Invalid name: {error}"));
            return;
        }
        let Some(parent) = src.parent() else {
            self.show_toast("The filesystem root cannot be renamed.");
            return;
        };
        let refresh = vec![parent.to_path_buf()];
        let dst = parent.join(&name);
        if dst == src {
            return;
        }
        let loc = self.file_tree_location.borrow().clone();
        let hosts = self.config.borrow().remote_hosts.clone();
        let clipboard_token =
            remote_fs::clipboard_token_for_location(&self.file_tree_clipboard.borrow(), &loc);
        // A renamed clipboard source would dangle; forget it on success.
        self.run_file_tree_op(
            intent,
            clipboard_token,
            vec![src.clone()],
            refresh,
            move || remote_fs::rename(&loc, &hosts, &src, &dst),
            |_| {},
            |_| {},
            sender,
        );
    }

    pub(crate) fn file_tree_delete_confirmed(
        &self,
        paths: Vec<std::path::PathBuf>,
        intent: file_tree::FileTreeIntent,
        sender: &ComponentSender<AppModel>,
    ) {
        if paths.is_empty() {
            return;
        }
        if !self.require_current_file_tree_intent(&intent) {
            return;
        }
        let loc = self.file_tree_location.borrow().clone();
        let hosts = self.config.borrow().remote_hosts.clone();
        let clipboard = self.file_tree_clipboard.clone();
        let clipboard_token = remote_fs::clipboard_token_for_location(&clipboard.borrow(), &loc);
        let mut refresh: Vec<std::path::PathBuf> = Vec::new();
        for path in &paths {
            if let Some(parent) = path.parent() {
                refresh.push(parent.to_path_buf());
            }
        }
        // Deleted clipboard sources would dangle; forget them on success.
        let toast_overlay = self.toast_overlay.clone();
        self.run_file_tree_op(
            intent,
            None,
            Vec::new(),
            refresh,
            move || Ok(remote_fs::delete_all(&loc, &hosts, &paths)),
            move |outcome| {
                remote_fs::retire_clipboard_sources(
                    &mut clipboard.borrow_mut(),
                    clipboard_token,
                    &outcome.succeeded,
                );
            },
            move |outcome| {
                if !outcome.summary.failed.is_empty() {
                    toast_overlay
                        .add_toast(adw::Toast::new(&batch_summary(&outcome.summary, "deleted")));
                }
            },
            sender,
        );
    }

    /// Paste the clipboard into `dir` (the root when None). Same location:
    /// cut becomes a rename, copy a recursive copy — per item for batches.
    /// Different locations: a streaming transfer (download, upload, or a
    /// local relay between two remote hosts), where cut copies first and
    /// then deletes the source per item.
    pub(crate) fn file_tree_paste(
        &self,
        dir: Option<std::path::PathBuf>,
        intent: file_tree::FileTreeIntent,
        clipboard_token: u64,
        sender: &ComponentSender<AppModel>,
    ) {
        if !self.require_current_file_tree_intent(&intent) {
            return;
        }
        let clip =
            remote_fs::clipboard_for_token(&self.file_tree_clipboard.borrow(), clipboard_token);
        let Some(clip) = clip else {
            self.show_toast("The file clipboard changed; reopen Paste.");
            return;
        };
        let loc = self.file_tree_location.borrow().clone();
        let dir = dir.unwrap_or_else(|| self.file_tree_root.borrow().clone());
        if dir.as_os_str().is_empty() {
            return;
        }
        let hosts = self.config.borrow().remote_hosts.clone();
        let cut = clip.cut;
        let count = clip.items.len();

        if clip.loc == loc {
            if count == 1 {
                // Single-item fast path with the plain rename/copy semantics.
                let src = clip.items[0].path.clone();
                let dst = remote_fs::paste_destination(&dir, &src);
                if dst == src {
                    self.show_toast("Copy and cut need a different target directory.");
                    return;
                }
                // A successful cut-paste consumes the clipboard.
                let refresh = if cut {
                    let mut dirs = vec![dir.clone()];
                    if let Some(parent) = src.parent() {
                        dirs.push(parent.to_path_buf());
                    }
                    dirs
                } else {
                    vec![dir.clone()]
                };
                let forget = if cut { vec![src.clone()] } else { Vec::new() };
                self.run_file_tree_op(
                    intent,
                    cut.then_some(clip.token),
                    forget,
                    refresh,
                    move || {
                        if cut {
                            remote_fs::rename(&loc, &hosts, &src, &dst)
                        } else {
                            remote_fs::copy(&loc, &hosts, &src, &dst)
                        }
                    },
                    |_| {},
                    |_| {},
                    sender,
                );
                return;
            }
            // Same-location batch: one worker job, per-item failures
            // collected into a summary toast.
            let items = clip.items.clone();
            let mut refresh = vec![dir.clone()];
            if cut {
                for item in &items {
                    if let Some(parent) = item.path.parent() {
                        refresh.push(parent.to_path_buf());
                    }
                }
            }
            let clip_loc = clip.loc.clone();
            let toast_overlay = self.toast_overlay.clone();
            let consumed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let consumed_for_worker = consumed.clone();
            let clipboard = self.file_tree_clipboard.clone();
            let clipboard_token = clip.token;
            self.run_file_tree_op(
                intent,
                None,
                Vec::new(),
                refresh,
                move || {
                    remote_fs::paste_all(
                        &hosts,
                        &clip_loc,
                        &items,
                        &loc,
                        &dir,
                        cut,
                        &remote_fs::TransferControl::new(),
                        &|_| {},
                        &|path| {
                            consumed_for_worker
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .push(path.to_path_buf());
                        },
                    )
                },
                move |_| {
                    let consumed = consumed
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    remote_fs::retire_clipboard_sources(
                        &mut clipboard.borrow_mut(),
                        cut.then_some(clipboard_token),
                        &consumed,
                    );
                },
                move |outcome| {
                    if !outcome.failed.is_empty() {
                        toast_overlay
                            .add_toast(adw::Toast::new(&batch_summary(&outcome, "pasted")));
                    }
                },
                sender,
            );
            return;
        }

        // Cross-location: stream through the probe (or the local relay).
        let src_loc = clip.loc.clone();
        let (verb, from_to) = match (&src_loc, &loc) {
            (remote_fs::FsLocation::Remote(_), remote_fs::FsLocation::Local) => {
                ("Downloading", format!("from {}", src_loc.label(&hosts)))
            }
            (remote_fs::FsLocation::Local, remote_fs::FsLocation::Remote(_)) => {
                ("Uploading", format!("to {}", loc.label(&hosts)))
            }
            _ => (
                "Relaying",
                format!("from {} to {}", src_loc.label(&hosts), loc.label(&hosts)),
            ),
        };
        let dst_loc = loc.clone();
        let control = remote_fs::TransferControl::new();
        let worker_control = control.clone();

        if count == 1 {
            // Single item: keep the byte-progress transfer toast.
            let item = clip.items[0].clone();
            let src = item.path.clone();
            let is_dir = item.is_dir;
            let name = src.file_name().map(file_tree::display_os_str);
            let name = name.unwrap_or_else(|| src.display().to_string());
            let name = review_input::safe_inline_display(&name, 256);
            // Uploads of one file can show "X / Y" from the local metadata;
            // downloads and relays have no trustworthy total.
            let total = if src_loc == remote_fs::FsLocation::Local && !is_dir {
                std::fs::metadata(&src).ok().map(|metadata| metadata.len())
            } else {
                None
            };
            let progress_label = {
                let verb = verb.to_string();
                let name = name.clone();
                move |bytes: u64| match total {
                    Some(total) => format!(
                        "{verb} {name}… {} / {}",
                        remote_fs::human_bytes(bytes),
                        remote_fs::human_bytes(total)
                    ),
                    None => format!("{verb} {name}… {}", remote_fs::human_bytes(bytes)),
                }
            };
            let busy = format!("{verb} {name} {from_to}…");
            let done = format!("{name}: transfer complete");
            let cancelled = format!("{name}: transfer cancelled");
            // Source deletion must use the same profile snapshot as the copy.
            // A settings reorder/edit while the transfer runs must never pair
            // the old index with a new host list.
            let cut_source = cut.then_some((src_loc.clone(), hosts.clone(), src.clone()));
            self.run_file_tree_transfer(
                intent,
                busy,
                cancelled,
                vec![dir.clone()],
                cut.then_some(clip.token),
                None,
                control,
                move |progress: &dyn Fn(u64)| {
                    remote_fs::transfer(
                        &hosts,
                        &src_loc,
                        &src,
                        &dst_loc,
                        &dir,
                        is_dir,
                        &worker_control,
                        progress,
                    )
                    .map(drop)
                },
                progress_label,
                move |()| (done, cut_source),
                sender,
            );
            return;
        }

        // Cross-location batch: one transfer job over all items; progress
        // reports completed items (remote sizes are unknown ahead of time).
        let items = clip.items.clone();
        let busy = format!("{verb} {count} items {from_to}…");
        let progress_label = {
            let busy = busy.clone();
            move |done: u64| format!("{busy} {done}/{count}")
        };
        let consumed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let consumed_for_worker = consumed.clone();
        self.run_file_tree_transfer(
            intent,
            busy,
            "Paste cancelled".to_string(),
            vec![dir.clone()],
            cut.then_some(clip.token),
            cut.then_some(consumed),
            control,
            move |progress: &dyn Fn(u64)| {
                remote_fs::paste_all(
                    &hosts,
                    &src_loc,
                    &items,
                    &dst_loc,
                    &dir,
                    cut,
                    &worker_control,
                    progress,
                    &|path| {
                        consumed_for_worker
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push(path.to_path_buf());
                    },
                )
            },
            progress_label,
            move |outcome| (batch_summary(&outcome, "pasted"), None),
            sender,
        );
    }

    /// Import OS file-manager drops onto the tree: plan per-item copy/upload
    /// with the guardrail caps, then run the batch as one cancellable
    /// transfer with progress. The batch finishes with a summary toast and an
    /// in-place refresh of the target directory.
    pub(crate) fn file_tree_import_paths(
        &self,
        paths: Vec<std::path::PathBuf>,
        dir: Option<std::path::PathBuf>,
        intent: file_tree::FileTreeIntent,
        sender: &ComponentSender<AppModel>,
    ) {
        if !self.require_current_file_tree_intent(&intent) {
            return;
        }
        let loc = self.file_tree_location.borrow().clone();
        let dir = dir.unwrap_or_else(|| self.file_tree_root.borrow().clone());
        if dir.as_os_str().is_empty() {
            return;
        }
        let hosts = self.config.borrow().remote_hosts.clone();
        let plan = match remote_fs::plan_drop(&paths, &loc, &dir) {
            Ok(plan) => plan,
            Err(rejection) => {
                self.show_toast(drop_rejection_message(&rejection, &loc, &hosts));
                return;
            }
        };
        let count = plan.items.len();
        let total = plan.total_bytes;
        let action = match &loc {
            remote_fs::FsLocation::Local => "Copying".to_string(),
            remote_fs::FsLocation::Remote(_) => format!("Uploading to {}", loc.label(&hosts)),
        };
        let busy = format!("{action}: {count} item(s)…");
        let progress_label = {
            let action = action.clone();
            move |bytes: u64| {
                format!(
                    "{action}: {count} item(s)… {} / {}",
                    remote_fs::human_bytes(bytes),
                    remote_fs::human_bytes(total)
                )
            }
        };
        let control = remote_fs::TransferControl::new();
        let worker_control = control.clone();
        self.run_file_tree_transfer(
            intent,
            busy,
            "Import cancelled".to_string(),
            vec![dir.clone()],
            None,
            None,
            control,
            move |progress: &dyn Fn(u64)| {
                remote_fs::run_drop(&plan, &loc, &hosts, &dir, &worker_control, progress)
            },
            progress_label,
            move |outcome| (batch_summary(&outcome, "imported"), None),
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

    /// Run one blocking op on a worker thread, then refresh only the affected
    /// directories in place on the GTK thread. `forget_clipboard` names
    /// clipboard sources that a successful op consumes or makes dangle.
    /// Backend/clipboard settlement always commits against the frozen token;
    /// UI publication is separately gated by the frozen tree authority.
    #[allow(clippy::too_many_arguments)]
    fn run_file_tree_op<T: Send + 'static>(
        &self,
        intent: file_tree::FileTreeIntent,
        clipboard_token: Option<u64>,
        forget_clipboard: Vec<std::path::PathBuf>,
        refresh: Vec<std::path::PathBuf>,
        op: impl FnOnce() -> std::io::Result<T> + Send + 'static,
        settle_success: impl FnOnce(&T) + 'static,
        on_current_success: impl FnOnce(T) + 'static,
        sender: &ComponentSender<AppModel>,
    ) {
        let toast_overlay = self.toast_overlay.clone();
        let clipboard = self.file_tree_clipboard.clone();
        let active_generation = self.file_tree_scan_generation.clone();
        let active_location = self.file_tree_location.clone();
        let active_config = self.config.clone();
        let transfer_revision = self.file_tree_transfer_revision.clone();
        let sender = sender.clone();
        if let Err(error) = file_tree::request_fs_op(op, move |result| match result {
            Ok(payload) => {
                if !forget_clipboard.is_empty() {
                    remote_fs::retire_clipboard_sources(
                        &mut clipboard.borrow_mut(),
                        clipboard_token,
                        &forget_clipboard,
                    );
                }
                // Settlement is about the backend result and the exact intent
                // token, so it must happen even if the user has since browsed
                // elsewhere. Only visible UI is suppressed for stale work.
                settle_success(&payload);
                let still_current = {
                    let location = active_location.borrow();
                    let config = active_config.borrow();
                    file_tree::file_tree_async_ui_is_current(
                        &intent,
                        active_generation.get(),
                        &location,
                        &config.remote_hosts,
                        None,
                        transfer_revision.get(),
                    )
                };
                if !still_current {
                    return;
                }
                sender.input(AppMsg::FileTreeOpSucceeded {
                    dirs: refresh,
                    intent: Box::new(intent),
                    transfer_id: None,
                });
                on_current_success(payload);
            }
            Err(error) => {
                let still_current = {
                    let location = active_location.borrow();
                    let config = active_config.borrow();
                    file_tree::file_tree_async_ui_is_current(
                        &intent,
                        active_generation.get(),
                        &location,
                        &config.remote_hosts,
                        None,
                        transfer_revision.get(),
                    )
                };
                if !still_current {
                    return;
                }
                let message = review_input::safe_inline_display(&error.to_string(), 512);
                toast_overlay.add_toast(adw::Toast::new(&format!(
                    "File operation failed: {message}"
                )));
            }
        }) {
            self.show_toast(format!("Could not start the file operation: {error}"));
        }
    }

    /// Run one cross-location transfer on a worker thread with a held busy
    /// toast that reports throttled progress and carries a Cancel action.
    /// Cancellation kills the transfer's children through the shared control
    /// and reports a neutral "cancelled" — not an error. On success, `finish`
    /// turns the worker's payload into the completion toast and an optional
    /// cut source, which is then deleted through the regular delete op; a
    /// failed source delete is reported as partial success.
    #[allow(clippy::too_many_arguments)]
    fn run_file_tree_transfer<T: Send + 'static>(
        &self,
        intent: file_tree::FileTreeIntent,
        busy_label: String,
        cancelled_label: String,
        refresh: Vec<std::path::PathBuf>,
        clipboard_token: Option<u64>,
        consumed_sources: Option<std::sync::Arc<std::sync::Mutex<Vec<std::path::PathBuf>>>>,
        control: remote_fs::TransferControl,
        transfer: impl FnOnce(&dyn Fn(u64)) -> std::io::Result<T> + Send + 'static,
        progress_label: impl Fn(u64) -> String + 'static,
        finish: impl FnOnce(
                T,
            ) -> (
                String,
                Option<(
                    remote_fs::FsLocation,
                    Vec<config::RemoteHost>,
                    std::path::PathBuf,
                )>,
            ) + 'static,
        sender: &ComponentSender<AppModel>,
    ) {
        let (busy_toast, transfer_id) = match self.file_tree_transfer_begin(&busy_label) {
            Ok(started) => started,
            Err(message) => {
                self.show_toast(message);
                return;
            }
        };
        // The Cancel action races the transfer's own completion harmlessly:
        // cancelling an already-finished control only kills exited children.
        busy_toast.set_button_label(Some("Cancel"));
        {
            let control = control.clone();
            busy_toast.connect_button_clicked(move |_| {
                control.cancel();
            });
        }
        let transfer_toast = self.file_tree_transfer_toast.clone();
        let toast_overlay = self.toast_overlay.clone();
        let clipboard = self.file_tree_clipboard.clone();
        let active_generation = self.file_tree_scan_generation.clone();
        let active_location = self.file_tree_location.clone();
        let active_config = self.config.clone();
        let transfer_revision = self.file_tree_transfer_revision.clone();
        let sender = sender.clone();
        let mut refresh = Some(refresh);
        let mut finish = Some(finish);
        let busy_toast_for_error = busy_toast.clone();
        let intent_for_start_error = intent.clone();
        if let Err(error) =
            file_tree::request_fs_op_streaming(transfer, move |outcome| match outcome {
                file_tree::FsOpOutcome::Progress(bytes) => {
                    let still_current = {
                        let location = active_location.borrow();
                        let config = active_config.borrow();
                        file_tree::file_tree_async_ui_is_current(
                            &intent,
                            active_generation.get(),
                            &location,
                            &config.remote_hosts,
                            Some(transfer_id),
                            transfer_revision.get(),
                        ) && transfer_toast.borrow().as_ref() == Some(&busy_toast)
                    };
                    if still_current {
                        busy_toast.set_title(&progress_label(bytes));
                    }
                }
                file_tree::FsOpOutcome::Done(result) => {
                    busy_toast.dismiss();
                    // Free the shared slot only if it still holds this
                    // transfer's toast; a newer one must survive.
                    {
                        let mut slot = transfer_toast.borrow_mut();
                        if slot.as_ref() == Some(&busy_toast) {
                            slot.take();
                        }
                    }
                    // Batch cut reports each source only after its move, or
                    // after a cross-location copy AND source delete, commits.
                    // Settle the completed prefix even when a later item was
                    // cancelled; the exact token protects a newer clipboard.
                    if let (Some(token), Some(consumed_sources)) =
                        (clipboard_token, consumed_sources.as_ref())
                    {
                        let consumed = consumed_sources
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .clone();
                        remote_fs::retire_clipboard_sources(
                            &mut clipboard.borrow_mut(),
                            Some(token),
                            &consumed,
                        );
                    }
                    match result {
                        Ok(payload) => {
                            let Some(finish) = finish.take() else { return };
                            let (done_label, cut_source) = finish(payload);
                            let still_current = {
                                let location = active_location.borrow();
                                let config = active_config.borrow();
                                file_tree::file_tree_async_ui_is_current(
                                    &intent,
                                    active_generation.get(),
                                    &location,
                                    &config.remote_hosts,
                                    Some(transfer_id),
                                    transfer_revision.get(),
                                )
                            };
                            if still_current {
                                toast_overlay.add_toast(adw::Toast::new(&done_label));
                                sender.input(AppMsg::FileTreeOpSucceeded {
                                    dirs: refresh.take().unwrap_or_default(),
                                    intent: Box::new(intent.clone()),
                                    transfer_id: Some(transfer_id),
                                });
                            }
                            let Some((loc, hosts, path)) = cut_source else {
                                return;
                            };
                            // A single cross-location cut consumes its source
                            // only after the delete commits. A failed delete
                            // leaves the captured clipboard item retryable.
                            let delete_toast_overlay = toast_overlay.clone();
                            let clipboard = clipboard.clone();
                            let path_for_retirement = path.clone();
                            let delete_intent = intent.clone();
                            let delete_generation = active_generation.clone();
                            let delete_location = active_location.clone();
                            let delete_config = active_config.clone();
                            let delete_revision = transfer_revision.clone();
                            if let Err(error) = file_tree::request_fs_op(
                                move || remote_fs::delete(&loc, &hosts, &path),
                                move |result| match result {
                                    Ok(()) => {
                                        remote_fs::retire_clipboard_sources(
                                            &mut clipboard.borrow_mut(),
                                            clipboard_token,
                                            &[path_for_retirement],
                                        );
                                    }
                                    Err(error) => {
                                        let still_current = {
                                            let location = delete_location.borrow();
                                            let config = delete_config.borrow();
                                            file_tree::file_tree_async_ui_is_current(
                                                &delete_intent,
                                                delete_generation.get(),
                                                &location,
                                                &config.remote_hosts,
                                                Some(transfer_id),
                                                delete_revision.get(),
                                            )
                                        };
                                        if !still_current {
                                            return;
                                        }
                                        let message = review_input::safe_inline_display(
                                            &error.to_string(),
                                            512,
                                        );
                                        delete_toast_overlay.add_toast(adw::Toast::new(&format!(
                                            "Copied, but deleting the source failed: {message}"
                                        )));
                                    }
                                },
                            ) {
                                log::warn!("failed to start cut-paste source delete: {error}");
                                let still_current = {
                                    let location = active_location.borrow();
                                    let config = active_config.borrow();
                                    file_tree::file_tree_async_ui_is_current(
                                        &intent,
                                        active_generation.get(),
                                        &location,
                                        &config.remote_hosts,
                                        Some(transfer_id),
                                        transfer_revision.get(),
                                    )
                                };
                                if still_current {
                                    let message =
                                        review_input::safe_inline_display(&error.to_string(), 512);
                                    toast_overlay.add_toast(adw::Toast::new(&format!(
                                        "Copied, but deleting the source could not start: {message}"
                                    )));
                                }
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                            // Cancellation is a neutral outcome, not an error;
                            // refresh so any partial state shows truthfully.
                            let still_current = {
                                let location = active_location.borrow();
                                let config = active_config.borrow();
                                file_tree::file_tree_async_ui_is_current(
                                    &intent,
                                    active_generation.get(),
                                    &location,
                                    &config.remote_hosts,
                                    Some(transfer_id),
                                    transfer_revision.get(),
                                )
                            };
                            if still_current {
                                toast_overlay.add_toast(adw::Toast::new(&cancelled_label));
                                sender.input(AppMsg::FileTreeOpSucceeded {
                                    dirs: refresh.take().unwrap_or_default(),
                                    intent: Box::new(intent.clone()),
                                    transfer_id: Some(transfer_id),
                                });
                            }
                        }
                        Err(error) => {
                            let still_current = {
                                let location = active_location.borrow();
                                let config = active_config.borrow();
                                file_tree::file_tree_async_ui_is_current(
                                    &intent,
                                    active_generation.get(),
                                    &location,
                                    &config.remote_hosts,
                                    Some(transfer_id),
                                    transfer_revision.get(),
                                )
                            };
                            if !still_current {
                                return;
                            }
                            let message =
                                review_input::safe_inline_display(&error.to_string(), 512);
                            toast_overlay
                                .add_toast(adw::Toast::new(&format!("Transfer failed: {message}")));
                        }
                    }
                }
            })
        {
            busy_toast_for_error.dismiss();
            let mut slot = self.file_tree_transfer_toast.borrow_mut();
            if slot.as_ref() == Some(&busy_toast_for_error) {
                slot.take();
            }
            drop(slot);
            let still_current = {
                let location = self.file_tree_location.borrow();
                let config = self.config.borrow();
                file_tree::file_tree_async_ui_is_current(
                    &intent_for_start_error,
                    self.file_tree_scan_generation.get(),
                    &location,
                    &config.remote_hosts,
                    Some(transfer_id),
                    self.file_tree_transfer_revision.get(),
                )
            };
            if still_current {
                self.show_toast(format!("Could not start the transfer: {error}"));
            }
        }
    }

    /// One held toast per transfer, dismissed when it finishes — the busy
    /// indication for payloads that can outlive a normal toast. Returns the
    /// toast so the completing transfer dismisses its own, not a newer one.
    fn file_tree_transfer_begin(&self, label: &str) -> Result<(adw::Toast, u64), &'static str> {
        let transfer_id = self
            .file_tree_transfer_revision
            .get()
            .checked_add(1)
            .ok_or("Could not start the transfer: transfer identity exhausted.")?;
        self.file_tree_transfer_revision.set(transfer_id);
        if let Some(old) = self.file_tree_transfer_toast.borrow_mut().take() {
            old.dismiss();
        }
        let toast = adw::Toast::new(label);
        toast.set_timeout(0);
        self.toast_overlay.add_toast(toast.clone());
        *self.file_tree_transfer_toast.borrow_mut() = Some(toast.clone());
        Ok((toast, transfer_id))
    }

    /// Apply a queued refresh only while both the tree authority and optional
    /// transfer identity are still current. This second gate covers a location
    /// or new-transfer change after the worker callback enqueued its message.
    pub(crate) fn file_tree_op_succeeded(
        &self,
        dirs: Vec<std::path::PathBuf>,
        intent: file_tree::FileTreeIntent,
        transfer_id: Option<u64>,
    ) {
        let location = self.file_tree_location.borrow();
        let config = self.config.borrow();
        let current = file_tree::file_tree_async_ui_is_current(
            &intent,
            self.file_tree_scan_generation.get(),
            &location,
            &config.remote_hosts,
            transfer_id,
            self.file_tree_transfer_revision.get(),
        );
        drop(config);
        drop(location);
        if current {
            self.refresh_tree_dirs(dirs);
        }
    }

    /// Refresh only the affected directories in place: locate each row by its
    /// path identity, merge a fresh scan into its children, and leave every
    /// other row — and all expansion — untouched. Directories that are not
    /// materialized in the model (collapsed, or outside the root) need no
    /// work: their lazy scan sees the fresh state on expansion.
    #[allow(deprecated)]
    pub(crate) fn refresh_tree_dirs(&self, dirs: Vec<std::path::PathBuf>) {
        let loc = self.file_tree_location.borrow().clone();
        let hosts = self.config.borrow().remote_hosts.clone();
        let root = self.file_tree_root.borrow().clone();
        let mut seen = std::collections::HashSet::new();
        for dir in dirs {
            if dir.as_os_str().is_empty() || !seen.insert(dir.clone()) {
                continue;
            }
            let parent_ref = if dir == root {
                None // the root merges at the model's top level
            } else {
                let Some(identity) = file_tree::encode_path_identity(&dir) else {
                    continue;
                };
                let Some(iter) = file_tree::find_row_by_identity(&self.file_tree_store, &identity)
                else {
                    continue;
                };
                // A placeholder child marks a never-expanded directory: its
                // lazy scan shows the fresh state anyway. An expanded-but-
                // empty directory has no children at all and still wants the
                // refresh.
                if let Some(first) = self.file_tree_store.iter_children(Some(&iter)) {
                    let placeholder: String = self
                        .file_tree_store
                        .get_value(&first, file_tree::COL_PATH as i32)
                        .get()
                        .unwrap_or_default();
                    if placeholder.is_empty() {
                        continue;
                    }
                }
                let Some(row_ref) = gtk::TreeRowReference::new(
                    &self.file_tree_store,
                    &self.file_tree_store.path(&iter),
                ) else {
                    continue;
                };
                Some(row_ref)
            };

            let store = self.file_tree_store.clone();
            let active_generation = self.file_tree_scan_generation.clone();
            let generation = active_generation.get();
            let active_location = self.file_tree_location.clone();
            let expected_loc = loc.clone();
            let active_root = self.file_tree_root.clone();
            let expected_root = root.clone();
            if let Err(error) =
                file_tree::request_dir_scan(loc.clone(), hosts.clone(), dir, move |result| {
                    if active_generation.get() != generation
                        || *active_location.borrow() != expected_loc
                        || *active_root.borrow() != expected_root
                    {
                        return;
                    }
                    let parent = parent_ref
                        .and_then(|row_ref| row_ref.path())
                        .and_then(|path| store.iter(&path));
                    match result {
                        Ok(entries) => {
                            file_tree::merge_refresh_children(&store, parent.as_ref(), entries)
                        }
                        Err(error) => log::warn!("failed to refresh directory rows: {error}"),
                    }
                })
            {
                log::warn!("failed to start directory refresh: {error}");
            }
        }
    }
}

/// Toast text for a wholesale-refused drop.
fn drop_rejection_message(
    rejection: &remote_fs::DropRejection,
    loc: &remote_fs::FsLocation,
    hosts: &[config::RemoteHost],
) -> String {
    let target = loc.label(hosts);
    match rejection {
        remote_fs::DropRejection::Empty => "Nothing to import.".to_string(),
        remote_fs::DropRejection::NotAbsolute(path) => format!(
            "Cannot import {}: not an absolute local path.",
            file_tree::display_full_path(path)
        ),
        remote_fs::DropRejection::Unreadable(path) => format!(
            "Cannot import {}: not readable.",
            file_tree::display_full_path(path)
        ),
        remote_fs::DropRejection::TooManyItems(count) => format!(
            "Refusing to import {count} items to {target}: the limit is {}.",
            remote_fs::MAX_DROP_ITEMS
        ),
        remote_fs::DropRejection::TooLarge(bytes) => format!(
            "Refusing to import to {target}: {} exceeds the {} transfer limit.",
            remote_fs::human_bytes(*bytes),
            remote_fs::human_bytes(remote_fs::MAX_TRANSFER_BYTES),
        ),
    }
}

/// Summary toast text for a multi-item batch: counts, plus the first
/// failure's reason so a partial batch is never silent.
fn batch_summary(outcome: &remote_fs::BatchOutcome, verb: &str) -> String {
    if outcome.failed.is_empty() {
        return format!("{} item(s) {verb}.", outcome.done);
    }
    let (name, error) = &outcome.failed[0];
    format!(
        "{} of {} failed ({}: {})",
        outcome.failed.len(),
        outcome.done + outcome.failed.len(),
        name,
        error
    )
}
