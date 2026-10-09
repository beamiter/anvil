#![allow(dead_code)]
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
mod config {
    #[derive(Clone, Debug, PartialEq)]
    pub struct RemoteHost {
        pub name: String,
    }
    pub struct Config {
        pub remote_hosts: Vec<RemoteHost>,
    }
}
mod review_input {
    pub fn safe_inline_display(s: &str, _: usize) -> String {
        s.into()
    }
}
mod remote_fs {
    #[derive(Clone, Debug, PartialEq)]
    pub enum FsLocation {
        Local,
        Remote(usize),
    }
    #[derive(Clone, Copy, Debug)]
    pub enum FsFailureKind {
        Superseded,
        Other,
    }
    pub fn remap_location_by_profile(
        l: &FsLocation,
        _: &[super::config::RemoteHost],
        _: &[super::config::RemoteHost],
    ) -> FsLocation {
        l.clone()
    }

    pub fn cancelled_error() -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled")
    }
    pub fn start_dir_with_cancellation(
        location: &FsLocation,
        hosts: &[super::config::RemoteHost],
        cancellation: &super::file_tree::ScanCancellation,
    ) -> std::io::Result<std::path::PathBuf> {
        if cancellation.is_cancelled() {
            return Err(cancelled_error());
        }
        start_dir(location, hosts)
    }
    pub fn start_dir(
        _: &FsLocation,
        _: &[super::config::RemoteHost],
    ) -> std::io::Result<std::path::PathBuf> {
        super::HOME_CALLS.with(|n| n.set(n.get() + 1));
        if super::CANCEL_PHASE.with(|n| n.get()) == 1 {
            super::ACTIVE_CANCEL.with(|c| c.borrow().as_ref().unwrap().cancel());
        }
        Ok("/remote/home".into())
    }
    pub fn list_dir_with_cancellation(
        _: &FsLocation,
        _: &[super::config::RemoteHost],
        _: &std::path::Path,
        _: &super::file_tree::ScanCancellation,
    ) -> std::io::Result<super::file_tree::DirectoryListing> {
        super::LIST_CALLS.with(|n| n.set(n.get() + 1));
        if super::CANCEL_PHASE.with(|n| n.get()) == 2 {
            super::ACTIVE_CANCEL.with(|c| c.borrow().as_ref().unwrap().cancel());
        }
        Ok(super::file_tree::DirectoryListing)
    }
    pub fn user_facing_failure_kind(_: FsFailureKind) -> &'static str {
        "failed"
    }
}
mod file_tree {
    use super::*;
    #[derive(Clone, Debug)]
    pub struct DirectoryListing;
    #[derive(Clone, Debug)]
    pub enum NavigationHistoryAction {
        Push,
    }
    #[derive(Clone, Debug, PartialEq)]
    pub struct FsAuthorityKey(remote_fs::FsLocation, Vec<config::RemoteHost>);
    impl FsAuthorityKey {
        pub fn capture(l: &remote_fs::FsLocation, h: &[config::RemoteHost]) -> Result<Self, ()> {
            Ok(Self(l.clone(), h.to_vec()))
        }
    }
    #[derive(Clone, Debug)]
    pub struct ObservedRemoteAuthority(pub config::RemoteHost);
    impl ObservedRemoteAuthority {
        pub fn current_location(
            &self,
            hosts: &[config::RemoteHost],
            _: &[String],
        ) -> Option<remote_fs::FsLocation> {
            (hosts.first() == Some(&self.0)).then_some(remote_fs::FsLocation::Remote(0))
        }
        pub fn profile(&self) -> &config::RemoteHost {
            &self.0
        }
    }
    #[derive(Clone, Debug)]
    pub struct FileTreeIntent {
        pub generation: u64,
        pub location: remote_fs::FsLocation,
    }
    fn file_tree_intent_is_current(
        i: &FileTreeIntent,
        g: u64,
        l: &remote_fs::FsLocation,
        _: &[config::RemoteHost],
    ) -> bool {
        i.generation == g && i.location == *l
    }
    #[derive(Clone, Debug, Default)]
    pub(crate) struct ScanCancellation(Arc<AtomicBool>);

    // @PRODUCTION_DEDUP@
    // @PRODUCTION_0@
    // @PRODUCTION_1@
    // @PRODUCTION_2@
    // @PRODUCTION_3@
    #[derive(Clone, Debug)]
    // @PRODUCTION_4@
    #[derive(Clone, Debug)]
    // @PRODUCTION_5@
    #[derive(Clone, Debug)]
    // @PRODUCTION_6@
}
#[derive(Clone)]
struct Command {
    argv: Vec<String>,
}
#[derive(Clone)]
struct Profile {
    identity: config::RemoteHost,
    execution_overlay: Vec<String>,
}
struct ComponentSender<T>(std::marker::PhantomData<T>);
struct Status;
impl Status {
    fn finish_success(&self, _: u64) {}
    fn finish_error_kind(&self, _: u64, _: remote_fs::FsFailureKind) {}
}
struct FailureGate;
impl FailureGate {
    fn record_success(&mut self, _: &file_tree::FsAuthorityKey, _: &PathBuf) {}
    fn record_failure_at(
        &mut self,
        _: file_tree::FsAuthorityKey,
        _: PathBuf,
        _: remote_fs::FsFailureKind,
        _: std::time::Instant,
    ) {
    }
}
struct AppModel {
    config: RefCell<config::Config>,
    file_tree_status: Status,
    file_tree_failure_gate: RefCell<FailureGate>,
    file_tree_navigation_revision: Cell<u64>,
    file_tree_ssh_detection_revision: Cell<u64>,
    file_tree_navigation_cancellation: RefCell<Option<file_tree::ScanCancellation>>,
    file_tree_user_operation_revision: Cell<u64>,
    file_tree_scan_generation: Cell<u64>,
    file_tree_location: RefCell<remote_fs::FsLocation>,
    file_tree_ssh_observation: Option<file_tree::SshFileTreeObservation>,
    live: Option<(u64, Command, Profile)>,
    commits: RefCell<Vec<remote_fs::FsLocation>>,
    pending: RefCell<Option<file_tree::PendingTreeNavigation>>,
    mutations: Cell<usize>,
}
impl AppModel {
    // @PRODUCTION_MANUAL_CANCEL@

    fn active_process_observed_ssh_profile(&self) -> Option<(u64, Command, Profile)> {
        self.live.clone()
    }
    fn sync_file_header_locations(&self) {}
    fn show_toast(&self, _: impl AsRef<str>) {
        self.mutations.set(self.mutations.get() + 1);
    }
    fn show_file_tree_ssh_failure(
        &self,
        _: u64,
        _: u64,
        _: impl AsRef<str>,
        _: &ComponentSender<AppModel>,
    ) {
        self.mutations.set(self.mutations.get() + 1);
    }
    fn commit_file_tree_navigation(
        &self,
        l: remote_fs::FsLocation,
        _: PathBuf,
        _: file_tree::DirectoryListing,
        _: Option<file_tree::DirectoryListing>,
        _: file_tree::NavigationHistoryAction,
    ) {
        self.commits.borrow_mut().push(l.clone());
        *self.file_tree_location.borrow_mut() = l;
    }
    fn stage_file_tree_navigation(
        &self,
        l: remote_fs::FsLocation,
        root: PathBuf,
        history: file_tree::NavigationHistoryAction,
        _: &ComponentSender<AppModel>,
    ) {
        *self.pending.borrow_mut() = Some(file_tree::PendingTreeNavigation {
            token: self.file_tree_navigation_revision.get(),
            location: l,
            hosts: self.config.borrow().remote_hosts.clone(),
            root,
            history,
            status_request: 1,
            cached: None,
        });
    }

    // @PRODUCTION_7@
    // @PRODUCTION_8@
    // @PRODUCTION_9@
    // @PRODUCTION_10@
    // @PRODUCTION_11@
    // @PRODUCTION_12@
}
fn setup() -> AppModel {
    let p = config::RemoteHost {
        name: "observed".into(),
    };
    let argv = vec!["ssh".into(), "observed".into()];
    let detection = file_tree::SshFileTreeDetection {
        cancellation: Default::default(),
        token: 7,
        pane_id: 44,
        observed: p.clone(),
        observed_argv: argv.clone(),
        execution_overlay: vec![],
        authority: file_tree::ObservedRemoteAuthority(p.clone()),
        tree_intent: file_tree::FileTreeIntent {
            generation: 5,
            location: remote_fs::FsLocation::Local,
        },
        preserve_tree: false,
        operation_revision: 3,
        resolved: false,
    };
    AppModel {
        config: RefCell::new(config::Config {
            remote_hosts: vec![p.clone()],
        }),
        file_tree_status: Status,
        file_tree_failure_gate: RefCell::new(FailureGate),
        file_tree_navigation_revision: Cell::new(11),
        file_tree_ssh_detection_revision: Cell::new(7),
        file_tree_navigation_cancellation: RefCell::new(None),
        file_tree_user_operation_revision: Cell::new(3),
        file_tree_scan_generation: Cell::new(5),
        file_tree_location: RefCell::new(remote_fs::FsLocation::Local),
        file_tree_ssh_observation: Some(file_tree::SshFileTreeObservation::Target(Box::new(
            detection,
        ))),
        live: Some((
            44,
            Command { argv },
            Profile {
                identity: p,
                execution_overlay: vec![],
            },
        )),
        commits: RefCell::new(vec![]),
        pending: RefCell::new(None),
        mutations: Cell::new(0),
    }
}
fn sender() -> ComponentSender<AppModel> {
    ComponentSender(std::marker::PhantomData)
}
fn prepare_listing(_: &mut AppModel) {}
fn finish_listing(app: &mut AppModel) {
    app.file_tree_ssh_probe_resolved(
        44,
        7,
        Ok(file_tree::SshFileTreeProbeResult {
            root: "/remote/home".into(),
            listing: Some(file_tree::DirectoryListing),
        }),
        &sender(),
    );
}

#[test]
fn current_combined_listing_publishes_synchronously() {
    let mut app = setup();
    prepare_listing(&mut app);
    finish_listing(&mut app);
    assert_eq!(
        *app.commits.borrow(),
        vec![remote_fs::FsLocation::Remote(0)]
    );
    assert!(app.pending.borrow().is_none());
}
#[test]
fn source_focus_change_during_listing_rejects_completion() {
    let mut app = setup();
    prepare_listing(&mut app);
    app.invalidate_file_tree_ssh_detection_context();
    finish_listing(&mut app);
    assert!(app.commits.borrow().is_empty());
    assert_eq!(app.mutations.get(), 0);
}
#[test]
fn source_exit_during_listing_rejects_completion() {
    let mut app = setup();
    prepare_listing(&mut app);
    app.live = None;
    finish_listing(&mut app);
    assert!(app.commits.borrow().is_empty());
}
#[test]
fn file_operation_during_listing_rejects_completion() {
    let mut app = setup();
    prepare_listing(&mut app);
    app.file_tree_user_operation_revision.set(4);
    finish_listing(&mut app);
    assert!(app.commits.borrow().is_empty());
}
#[test]
fn manual_navigation_stays_valid_after_source_focus_change() {
    let app = setup();
    let (token, _) = app.next_file_tree_navigation_token().unwrap();
    let request = file_tree::PendingTreeNavigation {
        token,
        location: remote_fs::FsLocation::Remote(0),
        hosts: app.config.borrow().remote_hosts.clone(),
        root: "/manual".into(),
        history: file_tree::NavigationHistoryAction::Push,
        status_request: 1,
        cached: None,
    };
    app.invalidate_file_tree_ssh_detection_context();
    app.file_tree_navigation_resolved(request, Ok(file_tree::DirectoryListing));
    assert_eq!(app.commits.borrow().len(), 1);
}
#[test]
fn manual_navigation_started_during_listing_revokes_automatic_request() {
    let mut app = setup();
    prepare_listing(&mut app);
    app.next_file_tree_navigation_token().unwrap();
    finish_listing(&mut app);
    assert!(app.commits.borrow().is_empty());
}

thread_local! {
 static HOME_CALLS:Cell<usize>=const{Cell::new(0)};
 static LIST_CALLS:Cell<usize>=const{Cell::new(0)};
 static CANCEL_PHASE:Cell<usize>=const{Cell::new(0)};
 static ACTIVE_CANCEL:RefCell<Option<file_tree::ScanCancellation>>=const{RefCell::new(None)};
}
fn probe_worker(
    same_location: bool,
    worker_cancellation: file_tree::ScanCancellation,
) -> std::io::Result<file_tree::SshFileTreeProbeResult> {
    let worker_location = remote_fs::FsLocation::Remote(0);
    ACTIVE_CANCEL.with(|c| *c.borrow_mut() = Some(worker_cancellation.clone()));
    let worker = // @PRODUCTION_WORKER@
    ;
    worker()
}
fn reset_worker(phase: usize) {
    HOME_CALLS.with(|c| c.set(0));
    LIST_CALLS.with(|c| c.set(0));
    CANCEL_PHASE.with(|c| c.set(phase));
}
#[test]
fn new_namespace_lists_inside_original_probe() {
    reset_worker(0);
    let r = probe_worker(false, Default::default()).unwrap();
    assert!(r.listing.is_some());
    HOME_CALLS.with(|c| assert_eq!(c.get(), 1));
    LIST_CALLS.with(|c| assert_eq!(c.get(), 1));
}
#[test]
fn same_namespace_probe_does_not_list_again() {
    reset_worker(0);
    let r = probe_worker(true, Default::default()).unwrap();
    assert!(r.listing.is_none());
    LIST_CALLS.with(|c| assert_eq!(c.get(), 0));
}
#[test]
fn cancellation_before_home_prevents_transport() {
    reset_worker(0);
    let c = file_tree::ScanCancellation::default();
    c.cancel();
    assert!(probe_worker(false, c).is_err());
    HOME_CALLS.with(|c| assert_eq!(c.get(), 0));
}
#[test]
fn cancellation_during_home_prevents_listing() {
    reset_worker(1);
    assert!(probe_worker(false, Default::default()).is_err());
    LIST_CALLS.with(|c| assert_eq!(c.get(), 0));
}
#[test]
fn cancellation_during_listing_discards_combined_result() {
    reset_worker(2);
    assert!(probe_worker(false, Default::default()).is_err());
    LIST_CALLS.with(|c| assert_eq!(c.get(), 1));
}

#[test]
fn manual_navigation_cancels_without_rearming_the_same_ssh_observation() {
    let app = setup();
    app.next_file_tree_navigation_token().unwrap();
    let (pane, command, profile) = app.live.as_ref().unwrap();
    assert!(file_tree::ssh_file_tree_observation_matches_target(
        app.file_tree_ssh_observation.as_ref(), app.file_tree_ssh_detection_revision.get(),
        *pane, &command.argv, &profile.identity, &profile.execution_overlay,
    ), "manual Files navigation must not re-arm the same SSH argv on the next heartbeat");
}
