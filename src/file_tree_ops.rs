//! File-tree root, navigation, location, and file-operation handlers.
//!
//! These GTK operations remain methods of the same Relm4 `AppModel` and keep the
//! existing file-tree store, header controller, and message routing unchanged.
//! Blocking filesystem work — local disk or the remote probe — always runs on
//! worker threads behind `file_tree`'s thread + mpsc + glib-poll skeleton.

use super::*;

/// The model destination captured before an asynchronous directory refresh.
/// Root is deliberately a distinct variant: a vanished non-root row must be
/// discarded, never collapsed into `None` and reinterpreted as the root.
enum FileTreeRefreshTarget {
    Root,
    Row {
        reference: gtk::TreeRowReference,
        identity: String,
    },
}

impl AppModel {
    fn next_file_tree_ssh_detection_token(&self) -> u64 {
        let token = self.file_tree_ssh_detection_revision.get().wrapping_add(1);
        self.file_tree_ssh_detection_revision.set(token);
        token
    }

    /// Revoke the authority of every queued SSH-to-Files result without
    /// requiring mutable access to the observation itself. The next poll sees
    /// the token mismatch and starts a fresh probe for the newly focused
    /// context, even after an A -> B -> A focus ABA.
    pub(crate) fn invalidate_file_tree_ssh_detection_context(&self) {
        self.next_file_tree_ssh_detection_token();
    }

    fn clear_file_tree_ssh_observation(&mut self) {
        if self.file_tree_ssh_observation.take().is_some() {
            self.next_file_tree_ssh_detection_token();
        }
    }

    pub(crate) fn clear_file_tree_ssh_observation_for_pane(&mut self, pane_id: u64) {
        let belongs_to_pane = self
            .file_tree_ssh_observation
            .as_ref()
            .is_some_and(|observation| match observation {
                file_tree::SshFileTreeObservation::Unsupported {
                    pane_id: observed, ..
                } => *observed == pane_id,
                file_tree::SshFileTreeObservation::Target(detection) => {
                    detection.pane_id == pane_id
                }
            });
        if belongs_to_pane {
            self.clear_file_tree_ssh_observation();
        }
    }

    fn active_process_observed_ssh_profile(
        &self,
    ) -> Option<(
        u64,
        process::ObservedSshCommand,
        file_tree::ObservedRemoteProfile,
    )> {
        let tab = self.tabs.get(self.active)?;
        let pane = tab.panes.get(tab.active_pane)?;
        if tab
            .remote
            .as_ref()
            .is_some_and(|remote| remote.pane_id == pane.id)
        {
            return None;
        }
        let command = pane.observed_ssh_command()?;
        let jterm_core::jsh_remote::ObservedSshTarget::Target(observed) = &command.target else {
            return None;
        };
        let profile = file_tree::observed_remote_profile(observed.clone())
            .ok()?
            .with_reusable_control_path(command.reusable_control_path.as_deref())
            .ok()?;
        Some((pane.id, command, profile))
    }

    fn show_file_tree_ssh_failure(
        &self,
        pane_id: u64,
        token: u64,
        message: impl AsRef<str>,
        sender: &ComponentSender<AppModel>,
    ) {
        let toast = adw::Toast::new(message.as_ref());
        toast.set_button_label(Some("Retry"));
        let sender = sender.clone();
        toast.connect_button_clicked(move |_| {
            sender.input(AppMsg::FileTreeSshRetry { pane_id, token });
        });
        self.toast_overlay.add_toast(toast);
    }

    /// Observe only the active pane's real foreground process argv. Terminal
    /// text and OSC command metadata never authorize a sidecar connection.
    /// A target is probed without disturbing the visible tree; the result has
    /// to survive all identity and navigation gates before it is committed.
    pub(crate) fn poll_active_ssh_file_tree(&mut self, sender: &ComponentSender<AppModel>) {
        if self.safe_mode {
            self.clear_file_tree_ssh_observation();
            return;
        }
        let Some((pane_id, managed_remote, command)) = self.tabs.get(self.active).and_then(|tab| {
            let pane = tab.panes.get(tab.active_pane)?;
            Some((
                pane.id,
                tab.remote
                    .as_ref()
                    .is_some_and(|remote| remote.pane_id == pane.id),
                pane.observed_ssh_command(),
            ))
        }) else {
            self.clear_file_tree_ssh_observation();
            return;
        };
        if managed_remote {
            self.clear_file_tree_ssh_observation_for_pane(pane_id);
            return;
        }
        let Some(command) = command else {
            self.clear_file_tree_ssh_observation_for_pane(pane_id);
            return;
        };

        let observed_profile = match &command.target {
            jterm_core::jsh_remote::ObservedSshTarget::NotSsh => {
                self.clear_file_tree_ssh_observation_for_pane(pane_id);
                return;
            }
            jterm_core::jsh_remote::ObservedSshTarget::Unsupported(reason) => {
                let already_reported = matches!(
                    self.file_tree_ssh_observation.as_ref(),
                    Some(file_tree::SshFileTreeObservation::Unsupported {
                        pane_id: observed_pane,
                        reason: observed_reason,
                    }) if *observed_pane == pane_id && *observed_reason == *reason
                );
                if !already_reported {
                    self.next_file_tree_ssh_detection_token();
                    self.file_tree_ssh_observation =
                        Some(file_tree::SshFileTreeObservation::Unsupported { pane_id, reason });
                    self.show_toast(format!(
                        "Files did not follow SSH: {reason}. Run an interactive SSH login, or choose a saved host in Files."
                    ));
                }
                return;
            }
            jterm_core::jsh_remote::ObservedSshTarget::Target(observed) => {
                match file_tree::observed_remote_profile(observed.clone()).and_then(|profile| {
                    profile.with_reusable_control_path(command.reusable_control_path.as_deref())
                }) {
                    Ok(profile) => profile,
                    Err(reason) => {
                        self.next_file_tree_ssh_detection_token();
                        self.file_tree_ssh_observation =
                            Some(file_tree::SshFileTreeObservation::Unsupported {
                                pane_id,
                                reason,
                            });
                        self.show_toast(format!(
                            "Files did not follow SSH: {reason}. Choose a saved host in Files."
                        ));
                        return;
                    }
                }
            }
        };
        let observed = observed_profile.identity;
        let execution_overlay = observed_profile.execution_overlay;

        let same_process_target = file_tree::ssh_file_tree_observation_matches_target(
            self.file_tree_ssh_observation.as_ref(),
            self.file_tree_ssh_detection_revision.get(),
            pane_id,
            &command.argv,
            &observed,
            &execution_overlay,
        );
        if same_process_target {
            return;
        }

        let (authority, tree_intent, same_location) = {
            let config = self.config.borrow();
            let location = self.file_tree_location.borrow();
            let authority =
                file_tree::observed_remote_authority(observed.clone(), &config.remote_hosts);
            let same_location = authority.matches_location(&location, &config.remote_hosts);
            (
                authority,
                file_tree::capture_file_tree_intent(
                    self.file_tree_scan_generation.get(),
                    &location,
                    &config.remote_hosts,
                ),
                same_location,
            )
        };
        let token = self.next_file_tree_ssh_detection_token();
        let operation_revision = self.file_tree_user_operation_revision.get();
        self.file_tree_ssh_observation = Some(file_tree::SshFileTreeObservation::Target(Box::new(
            file_tree::SshFileTreeDetection {
                token,
                pane_id,
                observed,
                observed_argv: command.argv,
                execution_overlay: execution_overlay.clone(),
                authority: authority.clone(),
                tree_intent,
                preserve_tree: same_location,
                operation_revision,
                resolved: false,
            },
        )));

        // Always probe through a value-owned location. Even a managed profile
        // may reorder while this worker is running; only the GTK-side commit
        // resolves its immutable full identity back through the live config.
        let probe_location = match authority.session_location(&execution_overlay) {
            Ok(location) => location,
            Err(error) => {
                if let Some(file_tree::SshFileTreeObservation::Target(detection)) =
                    self.file_tree_ssh_observation.as_mut()
                {
                    detection.resolved = true;
                }
                self.show_file_tree_ssh_failure(
                    pane_id,
                    token,
                    format!("Files could not prepare this SSH connection: {error}"),
                    sender,
                );
                return;
            }
        };

        // Even when the rows already name this stable namespace, a different
        // execution overlay is new authority and must prove connectivity on a
        // worker before it can replace the current immutable endpoint. The
        // callback preserves the existing root/rows after that proof.
        let callback_sender = sender.clone();
        let worker_location = probe_location.clone();
        if let Err(error) = file_tree::request_fs_op_at(
            &probe_location,
            &[],
            move || remote_fs::start_dir(&worker_location, &[]),
            move |result| {
                callback_sender.input(AppMsg::FileTreeSshProbeResolved {
                    pane_id,
                    token,
                    start: result.map_err(|error| remote_fs::classify_fs_error(&error)),
                });
            },
        ) {
            self.file_tree_ssh_probe_resolved(
                pane_id,
                token,
                Err(remote_fs::classify_fs_error(&error)),
                sender,
            );
        }
    }

    pub(crate) fn retry_file_tree_ssh_follow(
        &mut self,
        pane_id: u64,
        token: u64,
        sender: &ComponentSender<AppModel>,
    ) {
        let retry_is_current = file_tree::ssh_file_tree_retry_is_current(
            self.file_tree_ssh_observation.as_ref(),
            pane_id,
            token,
        );
        if !retry_is_current {
            return;
        }
        self.clear_file_tree_ssh_observation();
        self.poll_active_ssh_file_tree(sender);
    }

    pub(crate) fn file_tree_ssh_probe_resolved(
        &mut self,
        pane_id: u64,
        token: u64,
        start: Result<std::path::PathBuf, remote_fs::FsFailureKind>,
        sender: &ComponentSender<AppModel>,
    ) {
        let detection = match self.file_tree_ssh_observation.as_mut() {
            Some(file_tree::SshFileTreeObservation::Target(detection))
                if detection.pane_id == pane_id
                    && detection.token == token
                    && self.file_tree_ssh_detection_revision.get() == token
                    && !detection.resolved =>
            {
                detection.resolved = true;
                detection.clone()
            }
            _ => return,
        };

        // A process can exit between the worker's final read and this queued
        // GTK message. Re-read /proc now and require the same normalized argv.
        let Some((live_pane_id, live_command, live_profile)) =
            self.active_process_observed_ssh_profile()
        else {
            return;
        };
        let detection_is_current = {
            let config = self.config.borrow();
            let location = self.file_tree_location.borrow();
            file_tree::ssh_file_tree_detection_is_current(
                &detection,
                live_pane_id,
                &live_command.argv,
                &live_profile.identity,
                &live_profile.execution_overlay,
                self.file_tree_user_operation_revision.get(),
                self.file_tree_scan_generation.get(),
                &location,
                &config.remote_hosts,
            )
        };
        if !detection_is_current {
            return;
        }

        let root = match start {
            Ok(root) if root.is_absolute() => root,
            Ok(_) => {
                self.show_file_tree_ssh_failure(
                    pane_id,
                    token,
                    "SSH connected, but Files received an invalid remote home directory.",
                    sender,
                );
                return;
            }
            Err(error) => {
                let label = detection.authority.profile().name.as_str();
                let label = review_input::safe_inline_display(label, 256);
                let error = remote_fs::user_facing_failure_kind(error);
                self.show_file_tree_ssh_failure(
                    pane_id,
                    token,
                    format!(
                        "SSH is running, but Files could not open {label}: {error}. Use an SSH key/agent or choose a saved remote profile."
                    ),
                    sender,
                );
                return;
            }
        };

        let location = {
            let config = self.config.borrow();
            detection
                .authority
                .current_location(&config.remote_hosts, &detection.execution_overlay)
        };
        let Some(location) = location else {
            self.show_toast(
                "The matching remote profile changed while Files was connecting; the automatic switch was cancelled.",
            );
            return;
        };

        if detection.preserve_tree {
            *self.file_tree_location.borrow_mut() = location;
            self.sync_file_header_locations();
        } else {
            self.stage_file_tree_navigation(
                location,
                self.config.borrow().remote_hosts.clone(),
                root,
                file_tree::NavigationHistoryAction::Push,
                sender,
            );
        }
        self.set_sidebar_visible(true, false);
        self.sidebar_view.set(config::SidebarView::Files);
        self.apply_sidebar_view(config::SidebarView::Files, false);
    }

    fn next_file_tree_navigation_token(&self) -> Option<(u64, file_tree::ScanCancellation)> {
        let token = self.file_tree_navigation_revision.get().checked_add(1)?;
        self.file_tree_navigation_revision.set(token);
        if let Some(previous) = self.file_tree_navigation_cancellation.borrow_mut().take() {
            previous.cancel();
        }
        let cancellation = file_tree::ScanCancellation::default();
        *self.file_tree_navigation_cancellation.borrow_mut() = Some(cancellation.clone());
        Some((token, cancellation))
    }

    fn invalidate_pending_file_tree_navigation(&self) {
        self.file_tree_navigation_revision
            .set(self.file_tree_navigation_revision.get().wrapping_add(1));
        if let Some(previous) = self.file_tree_navigation_cancellation.borrow_mut().take() {
            previous.cancel();
        }
    }

    fn stage_file_tree_navigation(
        &self,
        location: remote_fs::FsLocation,
        hosts: Vec<config::RemoteHost>,
        root: std::path::PathBuf,
        history: file_tree::NavigationHistoryAction,
        sender: &ComponentSender<AppModel>,
    ) {
        if !root.is_absolute() {
            self.show_toast("Cannot open a non-absolute file-tree path.");
            return;
        }
        let authority = match file_tree::FsAuthorityKey::capture(&location, &hosts) {
            Ok(authority) => authority,
            Err(error) => {
                self.show_toast(remote_fs::user_facing_fs_error(&error));
                return;
            }
        };
        let Some((token, cancellation)) = self.next_file_tree_navigation_token() else {
            self.show_toast("File-tree navigation identity is exhausted; restart Anvil.");
            return;
        };
        let cached = self
            .file_tree_root_cache
            .borrow_mut()
            .get(&authority, &root);
        let status_request = self.file_tree_status.begin(
            file_tree::DirectoryScanTarget::Root(root.clone()),
            file_tree::DirectoryScanPhase::Loading,
        );
        let navigation = file_tree::PendingTreeNavigation {
            token,
            location: location.clone(),
            hosts: hosts.clone(),
            root: root.clone(),
            history,
            status_request,
            cached,
        };
        let callback_navigation = navigation.clone();
        let sender = sender.clone();
        let status = self.file_tree_status.clone();
        if let Err(error) = file_tree::request_dir_scan_cancellable(
            location,
            hosts,
            root,
            cancellation,
            move |queue_wait| status.mark_running(status_request, queue_wait),
            move |result| {
                sender.input(AppMsg::FileTreeNavigationResolved {
                    navigation: Box::new(callback_navigation),
                    listing: result.map_err(|error| remote_fs::classify_fs_error(&error)),
                });
            },
        ) {
            self.file_tree_navigation_resolved(
                navigation,
                Err(remote_fs::classify_fs_error(&error)),
            );
        }
    }

    pub(crate) fn file_tree_navigation_resolved(
        &self,
        navigation: file_tree::PendingTreeNavigation,
        listing: Result<file_tree::DirectoryListing, remote_fs::FsFailureKind>,
    ) {
        let current_hosts = self.config.borrow().remote_hosts.clone();
        let location = remote_fs::remap_location_by_profile(
            &navigation.location,
            &navigation.hosts,
            &current_hosts,
        );
        let expected_authority =
            file_tree::FsAuthorityKey::capture(&navigation.location, &navigation.hosts);
        let current_authority = file_tree::FsAuthorityKey::capture(&location, &current_hosts);
        let (Ok(expected_authority), Ok(current_authority)) =
            (expected_authority, current_authority)
        else {
            self.file_tree_status
                .finish_success(navigation.status_request);
            return;
        };
        if !file_tree::pending_navigation_is_current(
            navigation.token,
            self.file_tree_navigation_revision.get(),
            &expected_authority,
            &current_authority,
        ) {
            self.file_tree_status
                .finish_success(navigation.status_request);
            if navigation.token == self.file_tree_navigation_revision.get() {
                self.show_toast(
                    "The remote filesystem authority changed; navigation was cancelled.",
                );
                self.sync_file_header_locations();
            }
            return;
        }
        let authority = expected_authority;
        match listing {
            Ok(listing) => {
                self.file_tree_status
                    .finish_success(navigation.status_request);
                self.file_tree_failure_gate
                    .borrow_mut()
                    .record_success(&authority, &navigation.root);
                self.commit_file_tree_navigation(
                    location,
                    navigation.root,
                    listing,
                    navigation.cached,
                    navigation.history,
                );
            }
            Err(error) => {
                self.file_tree_status
                    .finish_error_kind(navigation.status_request, error);
                self.file_tree_failure_gate.borrow_mut().record_failure_at(
                    authority,
                    navigation.root,
                    error,
                    std::time::Instant::now(),
                );
                self.show_toast(format!(
                    "Cannot open directory: {}",
                    remote_fs::user_facing_failure_kind(error)
                ));
                self.sync_file_header_locations();
            }
        }
    }

    #[allow(deprecated)]
    fn commit_file_tree_navigation(
        &self,
        location: remote_fs::FsLocation,
        root: std::path::PathBuf,
        listing: file_tree::DirectoryListing,
        cached: Option<file_tree::DirectoryListing>,
        history: file_tree::NavigationHistoryAction,
    ) {
        self.file_header.emit(sidebar::FileHeaderMsg::CloseFilter);
        let generation = self.file_tree_scan_generation.get().wrapping_add(1);
        self.file_tree_scan_generation.set(generation);
        self.file_tree_refresh_revisions.borrow_mut().cancel_all();
        self.file_tree_status.reset();
        self.file_tree_snapshots.borrow_mut().reset();
        self.file_tree_store.clear();
        let display = if location == remote_fs::FsLocation::Local {
            file_tree::display_path(&root)
        } else {
            file_tree::display_full_path(&root)
        };
        self.file_header.emit(sidebar::FileHeaderMsg::SetRoot {
            display,
            tooltip: file_tree::display_full_path(&root),
            path: root.clone(),
        });
        *self.file_tree_location.borrow_mut() = location.clone();
        *self.file_tree_root.borrow_mut() = root.clone();
        self.sync_file_header_locations();

        let completed_at = listing.completed_at();
        let truncated = listing.truncated();
        let fresh_for_cache = listing.clone();
        let (fresh, _) = listing.into_parts();
        if let Some(cached) = cached {
            let (cached, _) = cached.into_parts();
            file_tree::append_entries(&self.file_tree_store, None, cached);
            let _ = file_tree::merge_refresh_children(&self.file_tree_store, None, fresh);
        } else {
            file_tree::append_entries(&self.file_tree_store, None, fresh);
        }
        self.file_tree_snapshots
            .borrow_mut()
            .record_success(root.clone(), completed_at);
        if let Ok(authority) =
            file_tree::FsAuthorityKey::capture(&location, &self.config.borrow().remote_hosts)
        {
            self.file_tree_root_cache
                .borrow_mut()
                .insert(authority, root.clone(), fresh_for_cache);
        }
        let history_hosts = if location == remote_fs::FsLocation::Local {
            Vec::new()
        } else {
            self.config.borrow().remote_hosts.clone()
        };
        let entry = file_tree::FileTreeHistoryEntry {
            location,
            hosts: history_hosts,
            root: root.clone(),
        };
        self.file_tree_navigation_history
            .borrow_mut()
            .commit(history, entry);
        self.file_tree_content_revision
            .set(self.file_tree_content_revision.get().wrapping_add(1));
        self.sync_file_tree_navigation_controls();
        self.file_tree_navigation_cancellation.borrow_mut().take();
        if truncated {
            log::warn!(
                "file-tree navigation retained only the first {} entries: {}",
                file_tree::MAX_DIRECTORY_ENTRIES,
                root.display()
            );
        }
    }

    fn sync_file_tree_navigation_controls(&self) {
        let location = self.file_tree_location.borrow();
        let hosts = self.config.borrow();
        let Ok(authority) = file_tree::FsAuthorityKey::capture(&location, &hosts.remote_hosts)
        else {
            self.file_header
                .emit(sidebar::FileHeaderMsg::SetNavigationAvailable {
                    back: false,
                    forward: false,
                });
            return;
        };
        let history = self.file_tree_navigation_history.borrow();
        self.file_header
            .emit(sidebar::FileHeaderMsg::SetNavigationAvailable {
                back: history.back(&authority).is_some(),
                forward: history.forward(&authority).is_some(),
            });
    }

    pub(crate) fn file_tree_go_back(&self, sender: &ComponentSender<AppModel>) {
        let location = self.file_tree_location.borrow().clone();
        let hosts = self.config.borrow().remote_hosts.clone();
        let Ok(authority) = file_tree::FsAuthorityKey::capture(&location, &hosts) else {
            return;
        };
        let previous = self.file_tree_navigation_history.borrow().back(&authority);
        if let Some((index, entry)) = previous {
            self.stage_file_tree_navigation(
                entry.location,
                entry.hosts,
                entry.root,
                file_tree::NavigationHistoryAction::MoveTo(index),
                sender,
            );
        }
    }

    pub(crate) fn file_tree_go_forward(&self, sender: &ComponentSender<AppModel>) {
        let location = self.file_tree_location.borrow().clone();
        let hosts = self.config.borrow().remote_hosts.clone();
        let Ok(authority) = file_tree::FsAuthorityKey::capture(&location, &hosts) else {
            return;
        };
        let next = self
            .file_tree_navigation_history
            .borrow()
            .forward(&authority);
        if let Some((index, entry)) = next {
            self.stage_file_tree_navigation(
                entry.location,
                entry.hosts,
                entry.root,
                file_tree::NavigationHistoryAction::MoveTo(index),
                sender,
            );
        }
    }

    pub(crate) fn file_tree_navigate_path(
        &self,
        path: std::path::PathBuf,
        sender: &ComponentSender<AppModel>,
    ) {
        if !path.is_absolute() {
            self.show_toast("Enter an absolute file-tree path.");
            return;
        }
        self.stage_file_tree_navigation(
            self.file_tree_location.borrow().clone(),
            self.config.borrow().remote_hosts.clone(),
            path,
            file_tree::NavigationHistoryAction::Push,
            sender,
        );
    }

    pub(crate) fn file_tree_path_entered(&self, text: String, sender: &ComponentSender<AppModel>) {
        match file_tree::validate_typed_file_tree_path(&text) {
            Ok(path) => {
                self.file_header
                    .emit(sidebar::FileHeaderMsg::ClosePathEntry);
                self.file_tree_navigate_path(path, sender);
            }
            Err(message) => self.show_toast(message),
        }
    }

    pub(crate) fn file_tree_open_path_entry(&self) {
        self.file_header.emit(sidebar::FileHeaderMsg::OpenPathEntry);
    }

    /// Rebuild the file tree with `root` at the top of the current location.
    /// An open filter is closed: the fresh rows would otherwise be invisible
    /// until the query is retyped.
    #[allow(deprecated)]
    pub(crate) fn set_file_tree_root(&self, root: std::path::PathBuf) {
        self.invalidate_pending_file_tree_navigation();
        self.file_header.emit(sidebar::FileHeaderMsg::CloseFilter);
        let generation = self.file_tree_scan_generation.get().wrapping_add(1);
        self.file_tree_scan_generation.set(generation);
        self.file_tree_refresh_revisions.borrow_mut().cancel_all();
        self.file_tree_status.reset();
        self.file_tree_snapshots.borrow_mut().reset();
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
            path: root.clone(),
        });
        *self.file_tree_root.borrow_mut() = root.clone();

        let hosts = self.config.borrow().remote_hosts.clone();
        let authority = file_tree::FsAuthorityKey::capture(&loc, &hosts).ok();
        let store = self.file_tree_store.clone();
        let active_generation = self.file_tree_scan_generation.clone();
        let active_root = self.file_tree_root.clone();
        let active_location = self.file_tree_location.clone();
        let expected_loc = loc.clone();
        let expected_root = root.clone();
        let status_request = self.file_tree_status.begin(
            file_tree::DirectoryScanTarget::Root(root.clone()),
            file_tree::DirectoryScanPhase::Loading,
        );
        let status_for_started = self.file_tree_status.clone();
        let status_for_result = self.file_tree_status.clone();
        let snapshots_for_result = self.file_tree_snapshots.clone();
        let cache_for_result = self.file_tree_root_cache.clone();
        let history_for_result = self.file_tree_navigation_history.clone();
        let failures_for_result = self.file_tree_failure_gate.clone();
        let hosts_for_history = hosts.clone();
        let authority_for_result = authority.clone();
        if let Err(error) = file_tree::request_dir_scan(
            loc,
            hosts,
            root,
            move |queue_wait| status_for_started.mark_running(status_request, queue_wait),
            move |result| {
                if active_generation.get() != generation
                    || *active_root.borrow() != expected_root
                    || *active_location.borrow() != expected_loc
                {
                    status_for_result.finish_success(status_request);
                    return;
                }
                match result {
                    Ok(listing) => {
                        let completed_at = listing.completed_at();
                        let listing_for_cache = listing.clone();
                        let (entries, truncated) = listing.into_parts();
                        file_tree::append_entries(&store, None, entries);
                        snapshots_for_result
                            .borrow_mut()
                            .record_success(expected_root.clone(), completed_at);
                        if let Some(authority) = authority_for_result.as_ref() {
                            failures_for_result
                                .borrow_mut()
                                .record_success(authority, &expected_root);
                            cache_for_result.borrow_mut().insert(
                                authority.clone(),
                                expected_root.clone(),
                                listing_for_cache,
                            );
                        }
                        history_for_result.borrow_mut().commit(
                            file_tree::NavigationHistoryAction::Push,
                            file_tree::FileTreeHistoryEntry {
                                location: expected_loc.clone(),
                                hosts: if expected_loc == remote_fs::FsLocation::Local {
                                    Vec::new()
                                } else {
                                    hosts_for_history.clone()
                                },
                                root: expected_root.clone(),
                            },
                        );
                        if truncated {
                            log::warn!(
                                "file-tree root retained only the first {} entries: {}",
                                file_tree::MAX_DIRECTORY_ENTRIES,
                                expected_root.display()
                            );
                        }
                        status_for_result.finish_success(status_request);
                    }
                    Err(error) => {
                        if let Some(authority) = authority_for_result.clone() {
                            failures_for_result.borrow_mut().record_failure_at(
                                authority,
                                expected_root.clone(),
                                remote_fs::classify_fs_error(&error),
                                std::time::Instant::now(),
                            );
                        }
                        status_for_result.finish_error(status_request, &error);
                        log::warn!(
                            "failed to scan file-tree root {}: {error}",
                            expected_root.display()
                        );
                    }
                }
            },
        ) {
            self.file_tree_status.finish_error(status_request, &error);
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
    pub(crate) fn file_tree_goto_current_cwd(&self, sender: &ComponentSender<AppModel>) {
        if let Some((loc, cwd)) = self.active_remote_cwd() {
            let reroot = {
                let current_location = self.file_tree_location.borrow();
                let current_root = self.file_tree_root.borrow();
                file_tree::file_tree_follow_requires_reroot(
                    &current_location,
                    &loc,
                    &current_root,
                    &cwd,
                )
            };
            if reroot {
                let hosts = self.config.borrow().remote_hosts.clone();
                self.stage_file_tree_navigation(
                    loc,
                    hosts,
                    cwd,
                    file_tree::NavigationHistoryAction::Push,
                    sender,
                );
            }
            return;
        }
        if *self.file_tree_location.borrow() != remote_fs::FsLocation::Local {
            return;
        }
        match self.active_cwd() {
            Some(dir) => {
                if *self.file_tree_root.borrow() != dir {
                    self.stage_file_tree_navigation(
                        remote_fs::FsLocation::Local,
                        self.config.borrow().remote_hosts.clone(),
                        dir,
                        file_tree::NavigationHistoryAction::Push,
                        sender,
                    );
                }
            }
            None => {
                if self.file_tree_root.borrow().as_os_str().is_empty() {
                    if let Some(home) = file_tree::home_dir() {
                        self.stage_file_tree_navigation(
                            remote_fs::FsLocation::Local,
                            self.config.borrow().remote_hosts.clone(),
                            home,
                            file_tree::NavigationHistoryAction::Push,
                            sender,
                        );
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
    pub(crate) fn file_tree_go_up(&self, sender: &ComponentSender<AppModel>) {
        let parent = self
            .file_tree_root
            .borrow()
            .parent()
            .map(std::path::Path::to_path_buf);
        if let Some(parent) = parent {
            self.stage_file_tree_navigation(
                self.file_tree_location.borrow().clone(),
                self.config.borrow().remote_hosts.clone(),
                parent,
                file_tree::NavigationHistoryAction::Push,
                sender,
            );
        }
    }

    /// Resolve Home against the filesystem authority currently shown by the
    /// tree. Remote homes are probed from the frozen complete profile rather
    /// than borrowed from the active terminal or a numeric config slot.
    pub(crate) fn file_tree_go_home(&self, sender: &ComponentSender<AppModel>) {
        let loc = self.file_tree_location.borrow().clone();
        if loc == remote_fs::FsLocation::Local {
            let home = file_tree::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));
            if *self.file_tree_root.borrow() != home {
                self.stage_file_tree_navigation(
                    loc,
                    self.config.borrow().remote_hosts.clone(),
                    home,
                    file_tree::NavigationHistoryAction::Push,
                    sender,
                );
            }
            return;
        }

        let hosts = self.config.borrow().remote_hosts.clone();
        let Some((token, cancellation)) = self.next_file_tree_navigation_token() else {
            self.show_toast("File-tree navigation identity is exhausted; restart Anvil.");
            return;
        };
        let intent =
            file_tree::capture_file_tree_intent(self.file_tree_scan_generation.get(), &loc, &hosts);
        let callback_intent = intent.clone();
        let callback_sender = sender.clone();
        let worker_loc = loc.clone();
        let worker_hosts = hosts.clone();
        if let Err(error) = file_tree::request_fs_op_cancellable_at(
            &loc,
            &hosts,
            cancellation,
            move || remote_fs::start_dir(&worker_loc, &worker_hosts),
            move |result| {
                callback_sender.input(AppMsg::FileTreeHomeResolved {
                    token,
                    intent: Box::new(callback_intent),
                    start: result.map_err(|error| remote_fs::classify_fs_error(&error)),
                });
            },
        ) {
            self.file_tree_home_resolved(
                token,
                intent,
                Err(remote_fs::classify_fs_error(&error)),
                sender,
            );
        }
    }

    /// Commit a Home probe only if its original backend and generation still
    /// match. Failure keeps the current tree and its last-good contents.
    pub(crate) fn file_tree_home_resolved(
        &self,
        token: u64,
        intent: file_tree::FileTreeIntent,
        start: Result<std::path::PathBuf, remote_fs::FsFailureKind>,
        sender: &ComponentSender<AppModel>,
    ) {
        let current = {
            let location = self.file_tree_location.borrow();
            let config = self.config.borrow();
            file_tree::home_navigation_is_current(
                token,
                self.file_tree_navigation_revision.get(),
                &intent,
                self.file_tree_scan_generation.get(),
                &location,
                &config.remote_hosts,
            )
        };
        if !current {
            return;
        }
        match start {
            Ok(home) if home.is_absolute() => {
                if *self.file_tree_root.borrow() != home {
                    self.stage_file_tree_navigation(
                        self.file_tree_location.borrow().clone(),
                        self.config.borrow().remote_hosts.clone(),
                        home,
                        file_tree::NavigationHistoryAction::Push,
                        sender,
                    );
                }
            }
            Ok(_) => self.show_toast(format!(
                "Cannot open filesystem home: {}",
                remote_fs::user_facing_failure_kind(remote_fs::FsFailureKind::InvalidResponse)
            )),
            Err(error) => self.show_toast(format!(
                "Cannot open filesystem home: {}",
                remote_fs::user_facing_failure_kind(error)
            )),
        }
    }

    /// Row activation enters an exact, still-materialized directory. The
    /// second model lookup keeps synthetic/stale messages from navigating to
    /// arbitrary paths even when they carry a once-valid authority token.
    #[allow(deprecated)]
    pub(crate) fn file_tree_enter_directory(
        &self,
        path: std::path::PathBuf,
        intent: file_tree::FileTreeIntent,
        sender: &ComponentSender<AppModel>,
    ) {
        if !self.require_current_file_tree_intent(&intent) {
            return;
        }
        let root = self.file_tree_root.borrow().clone();
        if !file_tree::directory_navigation_path_is_allowed(&root, &path) {
            return;
        }
        let Some(identity) = file_tree::encode_path_identity(&path) else {
            return;
        };
        let Some(iter) = file_tree::find_row_by_identity(&self.file_tree_store, &identity) else {
            return;
        };
        let is_dir = self
            .file_tree_store
            .get_value(&iter, file_tree::COL_IS_DIR as i32)
            .get::<bool>()
            .unwrap_or(false);
        if is_dir {
            self.stage_file_tree_navigation(
                self.file_tree_location.borrow().clone(),
                self.config.borrow().remote_hosts.clone(),
                path,
                file_tree::NavigationHistoryAction::Push,
                sender,
            );
        }
    }

    /// Reload the current root's rows in place: surviving rows keep their
    /// identity, so expansion everywhere else and the open filter survive —
    /// the same semantics every file operation's follow-up refresh uses.
    #[allow(deprecated)]
    pub(crate) fn file_tree_refresh(&self, intent: file_tree::FileTreeIntent) {
        if !self.require_current_file_tree_intent(&intent) {
            return;
        }
        let root = self.file_tree_root.borrow().clone();
        if root.as_os_str().is_empty() {
            self.init_file_tree();
        } else {
            let mut dirs = vec![root.clone()];
            dirs.extend(
                file_tree::expanded_directory_paths(
                    &self.file_tree_store,
                    &self.file_tree_view,
                    &self.file_tree_filter_model,
                )
                .into_iter()
                .filter(|path| path != &root)
                .take(file_tree::MAX_BULK_REFRESH_DIRS.saturating_sub(1)),
            );
            self.refresh_tree_dirs(dirs);
        }
    }

    pub(crate) fn file_tree_refresh_dirs(
        &self,
        dirs: Vec<std::path::PathBuf>,
        intent: file_tree::FileTreeIntent,
    ) {
        if self.require_current_file_tree_intent(&intent) {
            self.refresh_tree_dirs(dirs);
        }
    }

    /// Retry the exact directory represented by the visible status. Expanded
    /// failures retain their lazy placeholder; refresh/root failures keep their
    /// last-good rows and run the same in-place reconciliation path again.
    #[allow(deprecated)]
    pub(crate) fn file_tree_retry(
        &self,
        target: file_tree::DirectoryScanTarget,
        sender: &ComponentSender<AppModel>,
    ) {
        let status_target = target.clone();
        match target {
            file_tree::DirectoryScanTarget::Root(path) => {
                if *self.file_tree_root.borrow() == path {
                    self.refresh_tree_dirs_with_bypass(vec![path], true);
                } else {
                    let location = self.file_tree_location.borrow().clone();
                    let hosts = self.config.borrow().remote_hosts.clone();
                    let history = file_tree::FsAuthorityKey::capture(&location, &hosts)
                        .ok()
                        .map_or(file_tree::NavigationHistoryAction::Push, |authority| {
                            self.file_tree_navigation_history
                                .borrow()
                                .retry_action(&authority, &path)
                        });
                    self.stage_file_tree_navigation(location, hosts, path, history, sender);
                }
            }
            file_tree::DirectoryScanTarget::Refresh(path) => {
                let target_is_materialized = if *self.file_tree_root.borrow() == path {
                    true
                } else {
                    file_tree::encode_path_identity(&path)
                        .and_then(|identity| {
                            file_tree::find_row_by_identity(&self.file_tree_store, &identity)
                        })
                        .is_some_and(|iter| {
                            self.file_tree_store
                                .iter_children(Some(&iter))
                                .is_none_or(|first| {
                                    self.file_tree_store
                                        .get_value(&first, file_tree::COL_PATH as i32)
                                        .get::<String>()
                                        .is_ok_and(|identity| !identity.is_empty())
                                })
                        })
                };
                if target_is_materialized {
                    self.refresh_tree_dirs_with_bypass(vec![path], true);
                } else {
                    self.file_tree_status.dismiss_target(&status_target);
                }
            }
            file_tree::DirectoryScanTarget::Expand(path) => {
                let Some(identity) = file_tree::encode_path_identity(&path) else {
                    self.file_tree_status.dismiss_target(&status_target);
                    return;
                };
                let Some(iter) = file_tree::find_row_by_identity(&self.file_tree_store, &identity)
                else {
                    self.file_tree_status.dismiss_target(&status_target);
                    return;
                };
                let has_lazy_placeholder = self
                    .file_tree_store
                    .iter_children(Some(&iter))
                    .is_some_and(|first| {
                        self.file_tree_store
                            .get_value(&first, file_tree::COL_PATH as i32)
                            .get::<String>()
                            .is_ok_and(|identity| identity.is_empty())
                    });
                if !has_lazy_placeholder {
                    self.file_tree_status.dismiss_target(&status_target);
                    return;
                }
                let hosts = self.config.borrow().remote_hosts.clone();
                file_tree::on_expand(
                    &self.file_tree_store,
                    &iter,
                    &self.file_tree_scan_generation,
                    &self.file_tree_location,
                    hosts,
                    &self.file_tree_snapshots,
                    &self.file_tree_status,
                    &self.file_tree_failure_gate,
                    true,
                );
            }
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
            Ok(file_tree::FileTreeTerminalTarget::TemporarySsh(host)) => {
                if self.safe_mode {
                    self.show_toast("Remote connections are disabled in safe mode.");
                } else {
                    self.add_interactive_ssh_tab(&host, sender);
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

    /// Apply the hidden-file policy to the loaded model in place. This is a
    /// presentation preference, not navigation, so it does not invalidate
    /// pending filesystem operations or trigger remote probes.
    #[allow(deprecated)]
    pub(crate) fn file_tree_set_show_hidden(&self, show_hidden: bool) {
        let mut state = self.file_tree_filter.borrow_mut();
        file_tree::set_tree_show_hidden(&self.file_tree_filter_model, &mut state, show_hidden);
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
        // A session-only observed target exists in the dropdown only while it
        // is selected. Rebuild immediately after a manual move away so the
        // stale transient row cannot remain clickable during the home probe.
        self.sync_file_header_locations();
    }

    /// Resolve and list a selected filesystem location as one transaction.
    /// The old root, rows, expansion, and selection remain live until the
    /// exact authority's latest successful result commits on the GTK thread.
    fn begin_file_tree_location_switch(
        &self,
        loc: remote_fs::FsLocation,
        hosts: Vec<config::RemoteHost>,
        sender: &ComponentSender<AppModel>,
    ) {
        let Some((token, cancellation)) = self.next_file_tree_navigation_token() else {
            self.show_toast("File-tree navigation identity is exhausted; restart Anvil.");
            self.sync_file_header_locations();
            return;
        };
        let worker_loc = loc.clone();
        let worker_hosts = hosts.clone();
        let callback_location = loc.clone();
        let callback_hosts = hosts.clone();
        let callback_sender = sender.clone();
        let list_cancellation = cancellation.clone();
        if let Err(error) = file_tree::request_fs_op_cancellable_at(
            &loc,
            &hosts,
            cancellation,
            move || {
                let root = remote_fs::start_dir(&worker_loc, &worker_hosts)?;
                if !root.is_absolute() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "filesystem home is not absolute",
                    ));
                }
                let listing = remote_fs::list_dir_with_cancellation(
                    &worker_loc,
                    &worker_hosts,
                    &root,
                    &list_cancellation,
                )?;
                Ok((root, listing))
            },
            move |result| {
                callback_sender.input(AppMsg::FileTreeLocationResolved {
                    token,
                    location: callback_location,
                    hosts: callback_hosts,
                    result: result.map_err(|error| remote_fs::classify_fs_error(&error)),
                });
            },
        ) {
            self.file_tree_location_resolved(
                token,
                loc,
                hosts,
                Err(remote_fs::classify_fs_error(&error)),
            );
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
                remote_fs::FsLocation::Remote(_) | remote_fs::FsLocation::Transient(_) => {
                    let root = self.file_tree_root.borrow().clone();
                    if root.as_os_str().is_empty() {
                        self.begin_file_tree_location_switch(remapped, new_hosts.clone(), sender);
                    } else {
                        *self.file_tree_location.borrow_mut() = remapped;
                        self.invalidate_pending_file_tree_navigation();
                    }
                }
            }
        }
        self.sync_file_header_locations();
    }

    /// Finish a location switch on the GTK thread. Failure or any stale token
    /// only restores the selector; the last-good tree remains untouched.
    pub(crate) fn file_tree_location_resolved(
        &self,
        token: u64,
        location: remote_fs::FsLocation,
        hosts: Vec<config::RemoteHost>,
        result: Result<(std::path::PathBuf, file_tree::DirectoryListing), remote_fs::FsFailureKind>,
    ) {
        let current_hosts = self.config.borrow().remote_hosts.clone();
        let remapped = remote_fs::remap_location_by_profile(&location, &hosts, &current_hosts);
        let expected_authority = file_tree::FsAuthorityKey::capture(&location, &hosts);
        let remapped_authority = file_tree::FsAuthorityKey::capture(&remapped, &current_hosts);
        let (Ok(expected_authority), Ok(remapped_authority)) =
            (expected_authority, remapped_authority)
        else {
            return;
        };
        if !file_tree::pending_navigation_is_current(
            token,
            self.file_tree_navigation_revision.get(),
            &expected_authority,
            &remapped_authority,
        ) {
            if token != self.file_tree_navigation_revision.get() {
                return;
            }
            self.sync_file_header_locations();
            self.show_toast("The selected filesystem authority changed; navigation was cancelled.");
            return;
        }
        match result {
            Ok((root, listing)) if root.is_absolute() => {
                let authority = expected_authority;
                let cached = self
                    .file_tree_root_cache
                    .borrow_mut()
                    .get(&authority, &root);
                self.file_tree_failure_gate
                    .borrow_mut()
                    .record_success(&authority, &root);
                self.commit_file_tree_navigation(
                    remapped,
                    root,
                    listing,
                    cached,
                    file_tree::NavigationHistoryAction::Push,
                );
            }
            Ok(_) => {
                self.sync_file_header_locations();
                self.show_toast("Cannot browse filesystem: invalid directory response");
            }
            Err(error) => {
                self.sync_file_header_locations();
                if error == remote_fs::FsFailureKind::Superseded {
                    return;
                }
                let label = location.label(&hosts);
                self.show_toast(format!(
                    "Cannot browse {label}: {}",
                    remote_fs::user_facing_failure_kind(error)
                ));
            }
        }
    }

    /// Push the current labels + selection into the header's location
    /// selector after a config change or a rollback to Local.
    pub(crate) fn sync_file_header_locations(&self) {
        let hosts = self.config.borrow().remote_hosts.clone();
        let location = self.file_tree_location.borrow();
        let selected = match &*location {
            remote_fs::FsLocation::Local => 0,
            remote_fs::FsLocation::Remote(index) => (index + 1).min(hosts.len()),
            remote_fs::FsLocation::Transient(endpoint) if endpoint.is_managed() => endpoint
                .managed_profile()
                .and_then(|profile| config::unique_checked_remote_profile_index(&hosts, profile))
                .map_or(0, |index| index + 1),
            remote_fs::FsLocation::Transient(_) => hosts.len() + 1,
        };
        self.file_header.emit(sidebar::FileHeaderMsg::SetLocations {
            labels: remote_fs::location_labels_for(&hosts, &location),
            details: remote_fs::location_details_for(&hosts, &location),
            selected,
        });
    }

    fn file_tree_intent_is_current(&self, intent: &file_tree::FileTreeIntent) -> bool {
        let location = self.file_tree_location.borrow();
        let config = self.config.borrow();
        file_tree::file_tree_user_intent_is_current(
            intent,
            self.file_tree_content_revision.get(),
            self.file_tree_scan_generation.get(),
            &location,
            &config.remote_hosts,
        )
    }

    /// A stale dialog fails closed before name/path validation or any worker
    /// operation can reinterpret its targets against another filesystem.
    fn require_current_file_tree_intent(&self, intent: &file_tree::FileTreeIntent) -> bool {
        let current = self.file_tree_intent_is_current(intent);
        if current {
            self.file_tree_user_operation_revision
                .set(self.file_tree_user_operation_revision.get().wrapping_add(1));
        } else {
            self.show_toast(
                "The file-tree contents or location changed; the pending operation was cancelled.",
            );
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
        let clipboard_token = remote_fs::clipboard_token_for_location(
            &self.file_tree_clipboard.borrow(),
            &loc,
            &hosts,
        );
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
        let clipboard_token =
            remote_fs::clipboard_token_for_location(&clipboard.borrow(), &loc, &hosts);
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

        if remote_fs::locations_share_filesystem(&clip.loc, &loc, &hosts) {
            if count == 1 {
                let execution_loc =
                    remote_fs::direct_paste_execution_location(&hosts, &clip.loc, &loc)
                        .expect("same namespace has a direct execution endpoint")
                        .clone();
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
                            remote_fs::rename(&execution_loc, &hosts, &src, &dst)
                        } else {
                            remote_fs::copy(&execution_loc, &hosts, &src, &dst)
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
        let (verb, from_to) = if src_loc.is_remote() && loc == remote_fs::FsLocation::Local {
            ("Downloading", format!("from {}", src_loc.label(&hosts)))
        } else if src_loc == remote_fs::FsLocation::Local && loc.is_remote() {
            ("Uploading", format!("to {}", loc.label(&hosts)))
        } else {
            (
                "Relaying",
                format!("from {} to {}", src_loc.label(&hosts), loc.label(&hosts)),
            )
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
        let action = if loc.is_remote() {
            format!("Uploading to {}", loc.label(&hosts))
        } else {
            "Copying".to_string()
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
        self.file_tree_snapshots
            .borrow_mut()
            .mark_stale(refresh.iter());
        let toast_overlay = self.toast_overlay.clone();
        let clipboard = self.file_tree_clipboard.clone();
        let active_generation = self.file_tree_scan_generation.clone();
        let active_location = self.file_tree_location.clone();
        let active_config = self.config.clone();
        let transfer_revision = self.file_tree_transfer_revision.clone();
        let sender = sender.clone();
        let schedule_location = self.file_tree_location.borrow().clone();
        let schedule_hosts = self.config.borrow().remote_hosts.clone();
        let schedule_authority =
            file_tree::FsAuthorityKey::capture(&schedule_location, &schedule_hosts).ok();
        let root_cache = self.file_tree_root_cache.clone();
        let failure_gate = self.file_tree_failure_gate.clone();
        if let Err(error) =
            file_tree::request_fs_op_at(&schedule_location, &schedule_hosts, op, move |result| {
                match result {
                    Ok(payload) => {
                        if let Some(authority) = schedule_authority.as_ref() {
                            root_cache
                                .borrow_mut()
                                .invalidate(authority, refresh.iter());
                            for dir in &refresh {
                                failure_gate.borrow_mut().record_success(authority, dir);
                            }
                        }
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
                        let message = remote_fs::user_facing_fs_error(&error);
                        toast_overlay.add_toast(adw::Toast::new(&format!(
                            "File operation failed: {message}"
                        )));
                    }
                }
            })
        {
            self.show_toast(format!(
                "Could not start the file operation: {}",
                remote_fs::user_facing_fs_error(&error)
            ));
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
        self.file_tree_snapshots
            .borrow_mut()
            .mark_stale(refresh.iter());
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
        let schedule_location = self.file_tree_location.borrow().clone();
        let schedule_hosts = self.config.borrow().remote_hosts.clone();
        let schedule_authority =
            file_tree::FsAuthorityKey::capture(&schedule_location, &schedule_hosts).ok();
        let root_cache = self.file_tree_root_cache.clone();
        if let Err(error) = file_tree::request_fs_op_streaming_at(
            &schedule_location,
            &schedule_hosts,
            transfer,
            move |outcome| match outcome {
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
                    if let (Some(authority), Some(dirs)) =
                        (schedule_authority.as_ref(), refresh.as_ref())
                    {
                        root_cache.borrow_mut().invalidate(authority, dirs.iter());
                    }
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
                            let worker_loc = loc.clone();
                            let worker_hosts = hosts.clone();
                            if let Err(error) = file_tree::request_fs_op_at(
                                &loc,
                                &hosts,
                                move || remote_fs::delete(&worker_loc, &worker_hosts, &path),
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
                                        let message = remote_fs::user_facing_fs_error(&error);
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
                                    let message = remote_fs::user_facing_fs_error(&error);
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
                            let message = remote_fs::user_facing_fs_error(&error);
                            toast_overlay
                                .add_toast(adw::Toast::new(&format!("Transfer failed: {message}")));
                        }
                    }
                }
            },
        ) {
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
                self.show_toast(format!(
                    "Could not start the transfer: {}",
                    remote_fs::user_facing_fs_error(&error)
                ));
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
            if let Ok(authority) = file_tree::FsAuthorityKey::capture(
                &self.file_tree_location.borrow(),
                &self.config.borrow().remote_hosts,
            ) {
                self.file_tree_root_cache
                    .borrow_mut()
                    .invalidate(&authority, dirs.iter());
                for dir in &dirs {
                    self.file_tree_failure_gate
                        .borrow_mut()
                        .record_success(&authority, dir);
                }
            }
            self.refresh_tree_dirs(dirs);
        }
    }

    /// Revalidate only loaded directories while Files is visible and active.
    /// The root is considered first, expanded rows are deterministic, and the
    /// per-tick cap prevents a long-lived remote tree from flooding the queue.
    #[allow(deprecated)]
    pub(crate) fn file_tree_revalidate_due(&self) {
        if !self.sidebar_visible
            || self.sidebar_view.get() != config::SidebarView::Files
            || !self.window.is_active()
        {
            return;
        }
        let root = self.file_tree_root.borrow().clone();
        if root.as_os_str().is_empty() {
            return;
        }
        let mut candidates = vec![root.clone()];
        candidates.extend(
            file_tree::expanded_directory_paths(
                &self.file_tree_store,
                &self.file_tree_view,
                &self.file_tree_filter_model,
            )
            .into_iter()
            .filter(|path| path != &root),
        );
        let now = std::time::Instant::now();
        let due = self.file_tree_snapshots.borrow().due_paths_at(
            candidates,
            now,
            file_tree::MAX_TTL_REVALIDATE_DIRS,
        );
        let location = self.file_tree_location.borrow().clone();
        let hosts = self.config.borrow().remote_hosts.clone();
        let Ok(authority) = file_tree::FsAuthorityKey::capture(&location, &hosts) else {
            return;
        };
        let due: Vec<_> = due
            .into_iter()
            .filter(|path| {
                !self.file_tree_refresh_revisions.borrow().is_pending(path)
                    && self
                        .file_tree_failure_gate
                        .borrow()
                        .allows_auto_at(&authority, path, now)
            })
            .collect();
        self.file_tree_status.set_stale_count(due.len());
        if !due.is_empty() {
            self.refresh_tree_dirs(due);
        }
        self.file_tree_status.set_stale_count(0);
    }

    /// Refresh only the affected directories in place: locate each row by its
    /// path identity, merge a fresh scan into its children, and leave every
    /// other row — and all expansion — untouched. Directories that are not
    /// materialized in the model (collapsed, or outside the root) need no
    /// work: their lazy scan sees the fresh state on expansion.
    #[allow(deprecated)]
    pub(crate) fn refresh_tree_dirs(&self, dirs: Vec<std::path::PathBuf>) {
        self.refresh_tree_dirs_with_bypass(dirs, false);
    }

    #[allow(deprecated)]
    fn refresh_tree_dirs_with_bypass(
        &self,
        dirs: Vec<std::path::PathBuf>,
        bypass_failure_gate: bool,
    ) {
        let loc = self.file_tree_location.borrow().clone();
        let hosts = self.config.borrow().remote_hosts.clone();
        let Ok(authority) = file_tree::FsAuthorityKey::capture(&loc, &hosts) else {
            return;
        };
        let root = self.file_tree_root.borrow().clone();
        let mut seen = std::collections::HashSet::new();
        for dir in dirs {
            if dir.as_os_str().is_empty() || !seen.insert(dir.clone()) {
                continue;
            }
            if !bypass_failure_gate
                && !self.file_tree_failure_gate.borrow().allows_auto_at(
                    &authority,
                    &dir,
                    std::time::Instant::now(),
                )
            {
                continue;
            }
            self.file_tree_snapshots
                .borrow_mut()
                .mark_stale(std::iter::once(&dir));
            let target = if dir == root {
                FileTreeRefreshTarget::Root
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
                FileTreeRefreshTarget::Row {
                    reference: row_ref,
                    identity,
                }
            };
            let target_is_root = matches!(&target, FileTreeRefreshTarget::Root);

            let ticket = self.file_tree_refresh_revisions.borrow_mut().begin(&dir);
            let callback_ticket = ticket.clone();
            let start_error_ticket = ticket.clone();
            let refresh_revisions = self.file_tree_refresh_revisions.clone();
            let expected_dir = dir.clone();
            let start_error_dir = dir.clone();
            let status_request = self.file_tree_status.begin(
                file_tree::DirectoryScanTarget::Refresh(dir.clone()),
                file_tree::DirectoryScanPhase::Refreshing,
            );
            let status_for_started = self.file_tree_status.clone();
            let status_for_result = self.file_tree_status.clone();
            let snapshots_for_result = self.file_tree_snapshots.clone();
            let failures_for_result = self.file_tree_failure_gate.clone();
            let cache_for_result = self.file_tree_root_cache.clone();
            let authority_for_result = authority.clone();
            let store = self.file_tree_store.clone();
            let tree_view = self.file_tree_view.clone();
            let filter_model = self.file_tree_filter_model.clone();
            let content_revision = self.file_tree_content_revision.clone();
            let active_generation = self.file_tree_scan_generation.clone();
            let generation = active_generation.get();
            let active_location = self.file_tree_location.clone();
            let expected_loc = loc.clone();
            let active_root = self.file_tree_root.clone();
            let expected_root = root.clone();
            if let Err(error) = file_tree::request_dir_scan_cancellable(
                loc.clone(),
                hosts.clone(),
                dir,
                ticket.cancellation(),
                move |queue_wait| status_for_started.mark_running(status_request, queue_wait),
                move |result| {
                    if !refresh_revisions
                        .borrow_mut()
                        .finish_if_latest(&expected_dir, &callback_ticket)
                    {
                        status_for_result.finish_success(status_request);
                        return;
                    }
                    if active_generation.get() != generation
                        || *active_location.borrow() != expected_loc
                        || *active_root.borrow() != expected_root
                    {
                        status_for_result.finish_success(status_request);
                        return;
                    }
                    let parent = match target {
                        FileTreeRefreshTarget::Root => None,
                        FileTreeRefreshTarget::Row {
                            reference,
                            identity,
                        } => {
                            let Some(path) = reference.path() else {
                                status_for_result.finish_success(status_request);
                                return;
                            };
                            let Some(iter) = store.iter(&path) else {
                                status_for_result.finish_success(status_request);
                                return;
                            };
                            let current_identity: String = store
                                .get_value(&iter, file_tree::COL_PATH as i32)
                                .get()
                                .unwrap_or_default();
                            if !file_tree::refresh_row_identity_is_current(
                                &identity,
                                Some(&current_identity),
                            ) {
                                status_for_result.finish_success(status_request);
                                return;
                            }
                            Some(iter)
                        }
                    };
                    match result {
                        Ok(listing) => {
                            let completed_at = listing.completed_at();
                            let listing_for_cache = listing.clone();
                            let (entries, truncated) = listing.into_parts();
                            let selection = file_tree::capture_tree_selection(
                                &store,
                                &filter_model,
                                &tree_view,
                            );
                            let changed =
                                file_tree::merge_refresh_children(&store, parent.as_ref(), entries);
                            if changed {
                                content_revision.set(content_revision.get().wrapping_add(1));
                                tree_view.set_drag_dest_row(None, gtk::TreeViewDropPosition::After);
                                file_tree::restore_tree_selection(
                                    &store,
                                    &filter_model,
                                    &tree_view,
                                    selection,
                                );
                            }
                            if truncated {
                                log::warn!(
                                    "directory refresh retained only the first {} entries: {}",
                                    file_tree::MAX_DIRECTORY_ENTRIES,
                                    expected_dir.display()
                                );
                            }
                            snapshots_for_result
                                .borrow_mut()
                                .record_success(expected_dir.clone(), completed_at);
                            failures_for_result
                                .borrow_mut()
                                .record_success(&authority_for_result, &expected_dir);
                            if target_is_root {
                                cache_for_result.borrow_mut().insert(
                                    authority_for_result.clone(),
                                    expected_dir.clone(),
                                    listing_for_cache,
                                );
                            }
                            status_for_result.finish_success(status_request);
                        }
                        Err(error) => {
                            failures_for_result.borrow_mut().record_failure_at(
                                authority_for_result.clone(),
                                expected_dir.clone(),
                                remote_fs::classify_fs_error(&error),
                                std::time::Instant::now(),
                            );
                            status_for_result.finish_error(status_request, &error);
                            log::warn!("failed to refresh directory rows: {error}");
                        }
                    }
                },
            ) {
                let latest = self
                    .file_tree_refresh_revisions
                    .borrow_mut()
                    .finish_if_latest(&start_error_dir, &start_error_ticket);
                if latest {
                    self.file_tree_status.finish_error(status_request, &error);
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> remote_fs::FsLocation {
        remote_fs::FsLocation::Local
    }

    #[test]
    fn batch_summary_counts_successes_and_names_the_first_failure() {
        let clean = remote_fs::BatchOutcome {
            done: 3,
            failed: Vec::new(),
        };
        assert_eq!(batch_summary(&clean, "pasted"), "3 item(s) pasted.");

        let partial = remote_fs::BatchOutcome {
            done: 2,
            failed: vec![
                ("a.txt".to_string(), "denied".to_string()),
                ("b.txt".to_string(), "gone".to_string()),
            ],
        };
        assert_eq!(
            batch_summary(&partial, "deleted"),
            "2 of 4 failed (a.txt: denied)"
        );

        let single = remote_fs::BatchOutcome {
            done: 0,
            failed: vec![("only".to_string(), "boom".to_string())],
        };
        assert_eq!(
            batch_summary(&single, "imported"),
            "1 of 1 failed (only: boom)"
        );
    }

    #[test]
    fn drop_rejection_messages_name_the_reason_and_the_caps() {
        let loc = local();
        let hosts = Vec::new();
        assert_eq!(
            drop_rejection_message(&remote_fs::DropRejection::Empty, &loc, &hosts),
            "Nothing to import."
        );

        let relative = std::path::PathBuf::from("relative/file.txt");
        assert_eq!(
            drop_rejection_message(
                &remote_fs::DropRejection::NotAbsolute(relative.clone()),
                &loc,
                &hosts
            ),
            format!(
                "Cannot import {}: not an absolute local path.",
                file_tree::display_full_path(&relative)
            )
        );

        let unreadable = std::path::PathBuf::from("/tmp/anvil-ops-test-unreadable");
        assert_eq!(
            drop_rejection_message(
                &remote_fs::DropRejection::Unreadable(unreadable.clone()),
                &loc,
                &hosts
            ),
            format!(
                "Cannot import {}: not readable.",
                file_tree::display_full_path(&unreadable)
            )
        );

        assert_eq!(
            drop_rejection_message(&remote_fs::DropRejection::TooManyItems(300), &loc, &hosts),
            format!(
                "Refusing to import 300 items to Local: the limit is {}.",
                remote_fs::MAX_DROP_ITEMS
            )
        );

        let oversized = remote_fs::MAX_TRANSFER_BYTES + 1024 * 1024;
        assert_eq!(
            drop_rejection_message(&remote_fs::DropRejection::TooLarge(oversized), &loc, &hosts),
            format!(
                "Refusing to import to Local: {} exceeds the {} transfer limit.",
                remote_fs::human_bytes(oversized),
                remote_fs::human_bytes(remote_fs::MAX_TRANSFER_BYTES)
            )
        );
    }
}
