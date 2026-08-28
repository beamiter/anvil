//! Relm4 components for transient application dialogs.

use adw::prelude::*;
use relm4::adw;
use relm4::ComponentSender;

use crate::{AppModel, AppMsg};

pub(crate) mod ai_chat_store;
pub(crate) mod ai_panel;
pub(crate) mod command_palette;
pub(crate) mod debug_dashboard;
pub(crate) mod remote_picker;
pub(crate) mod settings;
pub(crate) mod tasks_panel;
pub(crate) mod workflow;

/// Confirm closing a tab/pane that has a running process (ssh, docker, nix
/// develop, …). On confirmation, dispatches `on_confirm` to force the close.
pub(crate) fn confirm_close(
    window: &adw::ApplicationWindow,
    running: &str,
    on_confirm: AppMsg,
    sender: &ComponentSender<AppModel>,
) {
    let body = format!("A process is still running here:\n\n{running}\n\nClose anyway?");
    let dialog = adw::AlertDialog::new(Some("Close with running process?"), Some(&body));
    dialog.add_responses(&[("cancel", "Cancel"), ("close", "Close")]);
    dialog.set_response_appearance("close", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    {
        let sender = sender.clone();
        dialog.connect_response(None, move |_, resp| {
            if resp == "close" {
                sender.input(on_confirm.clone());
            }
        });
    }
    dialog.present(Some(window));
}
