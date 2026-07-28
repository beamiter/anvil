//! Install and update surfaces for the companion shell, rsh.
//!
//! Two entry points, both explicit: the palette action, and a toast that
//! appears only after a background check found something actionable. Nothing
//! installs itself, and nothing blocks startup — the check runs on a worker
//! thread and reports back through the normal message loop.
//!
//! The decisions live in `jterm_core::rsh_install`, shared with the other
//! terminals; this file is only jterm1's surface for them.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use jterm_core::rsh_install::{self, Status};
use relm4::adw;
use relm4::gtk::glib;
use relm4::ComponentSender;

use crate::app_msg::AppMsg;
use crate::keybindings::Action;
use crate::AppModel;

/// How often the pending check result is polled from the worker thread.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Seconds the offer stays on screen. Long enough to read and act on, short
/// enough that ignoring it costs nothing; the palette action remains either way.
const TOAST_TIMEOUT: u32 = 12;

impl AppModel {
    /// Run the installer in its own tab. The script narrates what it does, so
    /// the tab is the progress UI — the user can read a failure or interrupt it
    /// with Ctrl+C like any other command.
    pub(crate) fn install_or_update_rsh(&mut self, sender: &ComponentSender<AppModel>) {
        match rsh_install::install_argv() {
            Ok(argv) => self.add_command_tab("Install rsh", argv, sender),
            Err(error) => {
                log::warn!("cannot stage the rsh installer: {error}");
                self.show_toast(format!("Could not write the installer script: {error}"));
            }
        }
    }

    /// Ask the installer what is published, off the main loop.
    pub(crate) fn start_rsh_update_check(&self, sender: &ComponentSender<AppModel>) {
        // "startup" asks the network every launch; "daily" reuses the
        // installer's cache, which every jterm on this machine shares.
        let Some(max_age) = self.config.borrow().rsh_update_check.max_age() else {
            return;
        };

        let slot: Arc<Mutex<Option<Status>>> = Arc::new(Mutex::new(None));
        let worker = slot.clone();
        std::thread::spawn(move || {
            *worker.lock().expect("rsh check slot poisoned") =
                Some(rsh_install::check_blocking(max_age));
        });

        let sender = sender.clone();
        glib::timeout_add_local(POLL_INTERVAL, move || {
            let Some(status) = slot.lock().expect("rsh check slot poisoned").take() else {
                return glib::ControlFlow::Continue;
            };
            sender.input(AppMsg::RshUpdateChecked(Box::new(status)));
            glib::ControlFlow::Break
        });
    }

    /// Turn a check result into an offer. A check that failed, or found nothing
    /// to do, stays silent: an offline laptop must not be nagged about a button
    /// that cannot work.
    pub(crate) fn offer_rsh_update(&self, status: &Status, sender: &ComponentSender<AppModel>) {
        if let Some(error) = &status.error {
            log::info!("rsh update check unavailable: {error}");
        }
        if let Some(other) = &status.shadowed_by {
            // Usually /usr/bin/rsh, the BSD remote shell. Installing does not
            // fix PATH order, so the installer explains it in the tab; here it
            // is only worth a log line.
            log::warn!("PATH resolves rsh to {other}, which jterm1 does not manage");
        }

        let Some(prompt) = rsh_install::prompt_for(status) else {
            return;
        };
        log::info!("rsh notice: {}", prompt.banner_title());

        let toast = adw::Toast::new(&prompt.banner_title());
        toast.set_button_label(Some(prompt.button_label()));
        toast.set_timeout(TOAST_TIMEOUT);
        let sender = sender.clone();
        toast.connect_button_clicked(move |_| sender.input(AppMsg::Action(Action::InstallRsh)));
        self.toast_overlay.add_toast(toast);
    }
}
