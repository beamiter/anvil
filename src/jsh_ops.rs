//! Install and update surfaces for the companion shell, jsh.
//!
//! Two entry points, both explicit: the palette action, and a toast that
//! appears only after a background check found something actionable. Nothing
//! installs itself, and nothing blocks startup — the check runs on a worker
//! thread and reports back through the normal message loop.
//!
//! The decisions live in `jterm_core::jsh_install`, shared with the other
//! terminals; this file is only jterm1's surface for them.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use jterm_core::jsh_install::{self, Status};
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
    pub(crate) fn install_or_update_jsh(&mut self, sender: &ComponentSender<AppModel>) {
        if self.safe_mode {
            self.show_toast("jsh installation is unavailable in safe mode.");
            return;
        }
        match jsh_install::install_argv() {
            Ok(argv) => self.add_command_tab("Install jsh", argv, sender),
            Err(error) => {
                let error =
                    crate::text_safety::bounded_display_text(&error.to_string(), 2 * 1024, false);
                log::warn!("cannot stage the jsh installer: {error}");
                self.show_toast(format!("Could not write the installer script: {error}"));
            }
        }
    }

    /// Ask the installer what is published, off the main loop.
    pub(crate) fn start_jsh_update_check(&self, sender: &ComponentSender<AppModel>) {
        // Defense in depth with Config::safe_defaults(): even a future config
        // refactor must not turn the isolated recovery session into a network
        // or shared-cache producer.
        if self.safe_mode {
            return;
        }
        // "startup" asks the network every launch; "daily" reuses the
        // installer's cache, which every jterm on this machine shares.
        let Some(max_age) = self.config.borrow().jsh_update_check.max_age() else {
            return;
        };

        let slot: Arc<Mutex<Option<Status>>> = Arc::new(Mutex::new(None));
        let worker = slot.clone();
        let spawn_result = std::thread::Builder::new()
            .name("jterm1-jsh-update-check".to_string())
            .spawn(move || {
                *worker.lock().expect("jsh check slot poisoned") =
                    Some(jsh_install::check_blocking(max_age));
            });
        if let Err(error) = spawn_result {
            *slot.lock().expect("jsh check slot poisoned") = Some(Status {
                error: Some(format!("could not start update-check worker: {error}")),
                ..Status::default()
            });
        }

        let sender = sender.clone();
        glib::timeout_add_local(POLL_INTERVAL, move || {
            let Some(status) = slot.lock().expect("jsh check slot poisoned").take() else {
                return glib::ControlFlow::Continue;
            };
            sender.input(AppMsg::JshUpdateChecked(Box::new(status)));
            glib::ControlFlow::Break
        });
    }

    /// Turn a check result into an offer. A check that failed, or found nothing
    /// to do, stays silent: an offline laptop must not be nagged about a button
    /// that cannot work.
    pub(crate) fn offer_jsh_update(&self, status: &Status, sender: &ComponentSender<AppModel>) {
        if let Some(error) = &status.error {
            let error = crate::text_safety::bounded_display_text(error, 2 * 1024, false);
            log::info!("jsh update check unavailable: {error}");
        }
        if let Some(other) = &status.shadowed_by {
            // Some other binary named jsh, earlier on PATH. Installing does not
            // fix PATH order, so the installer explains it in the tab; here it
            // is only worth a log line.
            let other = crate::text_safety::bounded_display_text(other, 2 * 1024, false);
            log::warn!("PATH resolves jsh to {other}, which jterm1 does not manage");
        }

        let Some(prompt) = jsh_install::prompt_for(status) else {
            return;
        };
        let title = crate::text_safety::bounded_display_text(&prompt.banner_title(), 1024, false);
        let title = if title.is_empty() {
            "jsh update available".to_string()
        } else {
            title
        };
        let button = crate::text_safety::bounded_display_text(prompt.button_label(), 128, false);
        log::info!("jsh notice: {title}");

        let toast = adw::Toast::new(&title);
        toast.set_button_label(Some(&button));
        toast.set_timeout(TOAST_TIMEOUT);
        let sender = sender.clone();
        toast.connect_button_clicked(move |_| sender.input(AppMsg::Action(Action::InstallJsh)));
        self.toast_overlay.add_toast(toast);
    }
}
