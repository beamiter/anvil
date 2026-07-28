//! Natural-language command palette and session-level AI panel operations.
//!
//! These are inherent methods on the existing Relm4 `AppModel`; AI requests still
//! return through the same `AppMsg` input channel and active terminal controller.

use super::*;

/// Strip one layer of markdown code fence (```bash … ``` or ``` … ```) if it
/// wraps the entire response. LLMs often format single-command outputs that
/// way even when asked for raw text.
fn strip_code_fences(s: &str) -> &str {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("```") {
        let after_lang = rest.split_once('\n').map(|(_, r)| r).unwrap_or(rest);
        if let Some(inner) = after_lang.trim_end().strip_suffix("```") {
            return inner.trim();
        }
    }
    s
}

/// Convert a model response into text that is safe to place in the live shell
/// editor. Newlines and terminal control characters are rejected rather than
/// normalised: feeding either into the PTY could execute input even though the
/// palette promises to insert a command for review without submitting it.
fn command_for_review(response: &str) -> Result<String, &'static str> {
    let command = strip_code_fences(response).trim();
    if command.is_empty() {
        return Err("AI returned an empty command; nothing was inserted.");
    }
    if command.len() > 16 * 1024 {
        return Err("AI returned an oversized command; nothing was inserted.");
    }
    if command.chars().any(char::is_control) {
        return Err("AI returned a multi-line or control-character command; nothing was inserted.");
    }
    Ok(command.to_string())
}

impl AppModel {
    /// `?` palette accept handler: run the natural-language query through the
    /// configured AI provider. The initiating pane is captured before the
    /// request and every result requires an explicit Insert/Cancel review.
    pub(crate) fn handle_palette_ask_ai(&self, query: String, sender: &ComponentSender<AppModel>) {
        if self.safe_mode {
            self.show_toast("AI is unavailable in safe mode.");
            return;
        }
        let client = match ai::client_from_config(&self.config.borrow()) {
            Ok(client) => client,
            Err(error) => {
                log::warn!("AI palette: {error}");
                self.show_toast(format!("AI provider is unavailable: {error}"));
                return;
            }
        };
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let Some(pane) = tab.panes.get(tab.active_pane) else {
            return;
        };
        if !matches!(pane.mode, TerminalMode::Block) || !pane.terminal.can_accept_agent_command() {
            self.show_toast("AI command generation requires an idle, empty Block-mode prompt.");
            return;
        }
        let pane_id = pane.id;
        let cwd = pane.cwd.clone().unwrap_or_else(|| "<unknown>".to_string());
        let (system, user) = ai::build_nl_to_cmd_prompt(&query, &cwd);
        let sender_clone = sender.clone();

        // The worker and GLib timer each retain their own cancellation token,
        // so the callback remains live after this temporary handle is dropped.
        // Avoid `mem::forget`: every palette request used to leak one token.
        let _request = ai::ask(client, system, user, move |result| match result {
            Ok(response) => match command_for_review(&response) {
                Ok(command) => {
                    sender_clone.input(AppMsg::PaletteReviewAiCommand { pane_id, command })
                }
                Err(message) => {
                    log::warn!("AI palette rejected unsafe response: {message}");
                    sender_clone.input(AppMsg::Toast(message.to_string()));
                }
            },
            Err(e) => {
                log::warn!("AI palette request failed: {e}");
                sender_clone.input(AppMsg::Toast(format!("AI request failed: {e}")));
            }
        });
    }

    pub(crate) fn review_palette_ai_command(
        &self,
        pane_id: u64,
        command: String,
        sender: &ComponentSender<AppModel>,
    ) {
        if !self.ai_command_target_is_ready(pane_id) {
            self.show_toast(
                "The target Block prompt is busy, contains input, or no longer exists; the generated command was not inserted.",
            );
            return;
        }
        let danger = agent::is_dangerous(&command);
        let title = if danger.is_some() {
            "Review potentially destructive command"
        } else {
            "Review generated command"
        };
        let body = danger.as_ref().map_or_else(
            || format!("{command}\n\nInsert it at the prompt without running it?"),
            |reason| {
                format!(
                    "Warning: {reason}.\n\n{command}\n\nInsert it at the prompt without running it?"
                )
            },
        );
        let dialog = adw::AlertDialog::new(Some(title), Some(&body));
        dialog.add_responses(&[("cancel", "Cancel"), ("insert", "Insert")]);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.set_response_appearance(
            "insert",
            if danger.is_some() {
                adw::ResponseAppearance::Destructive
            } else {
                adw::ResponseAppearance::Suggested
            },
        );
        let sender = sender.clone();
        dialog.connect_response(None, move |_, response| {
            if response == "insert" {
                sender.input(AppMsg::PaletteInsertAiCommand {
                    pane_id,
                    command: command.clone(),
                });
            }
        });
        dialog.present(Some(&self.window));
    }

    pub(crate) fn insert_palette_ai_command(&self, pane_id: u64, command: String) {
        if !self.ai_command_target_is_ready(pane_id) {
            self.show_toast(
                "The target Block prompt is busy, contains input, or no longer exists; the generated command was not inserted.",
            );
            return;
        }
        self.insert_review_text_into_pane(pane_id, &command);
    }

    fn ai_command_target_is_ready(&self, pane_id: u64) -> bool {
        let Some((tab_index, pane_index)) = self.find_pane(pane_id) else {
            return false;
        };
        let pane = &self.tabs[tab_index].panes[pane_index];
        matches!(pane.mode, TerminalMode::Block) && pane.terminal.can_accept_agent_command()
    }

    /// Open the session-level AI panel with the configured history source.
    pub(crate) fn show_ai_session_panel(&self) {
        self.show_ai_session_panel_with_context(None);
    }

    pub(crate) fn show_ai_session_panel_with_context(
        &self,
        initial_context: Option<ai::BlockContext>,
    ) {
        if self.safe_mode {
            self.show_toast("AI is unavailable in safe mode.");
            return;
        }
        let client = match ai::client_from_config(&self.config.borrow()) {
            Ok(client) => client,
            Err(error) => {
                self.show_toast(format!("AI provider is unavailable: {error}"));
                return;
            }
        };
        self.ai_panel.emit(dialogs::ai_panel::AiPanelMsg::Open {
            history_path: self.config.borrow().command_history_path.clone(),
            client,
            stream: self.config.borrow().ai_stream,
            initial_context,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{command_for_review, strip_code_fences};

    #[test]
    fn strips_a_wrapping_language_fence() {
        assert_eq!(
            strip_code_fences("```bash\nprintf 'ok'\n```"),
            "printf 'ok'"
        );
    }

    #[test]
    fn accepts_one_printable_command() {
        assert_eq!(
            command_for_review("  printf 'ok'  ").unwrap(),
            "printf 'ok'"
        );
    }

    #[test]
    fn rejects_newlines_that_could_submit_shell_input() {
        assert!(command_for_review("echo first\necho second").is_err());
        assert!(command_for_review("echo first\recho second").is_err());
    }

    #[test]
    fn rejects_terminal_control_characters() {
        assert!(command_for_review("echo ok\u{1b}[2J").is_err());
        assert!(command_for_review("echo\tok").is_err());
    }

    #[test]
    fn rejects_oversized_model_output() {
        assert!(command_for_review(&"x".repeat(16 * 1024 + 1)).is_err());
    }
}
