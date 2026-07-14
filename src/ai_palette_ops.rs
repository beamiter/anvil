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
    if command.chars().any(char::is_control) {
        return Err(
            "AI returned a multi-line or control-character command; nothing was inserted.",
        );
    }
    Ok(command.to_string())
}

impl AppModel {
    /// `?` palette accept handler: run the natural-language query through the
    /// configured AI provider and, on success, type the returned command into
    /// the active pane (no autosubmit). Errors raise a transient toast/log
    /// only — the user can always retry.
    pub(crate) fn handle_palette_ask_ai(&self, query: String, sender: &ComponentSender<AppModel>) {
        if !self.config.borrow().ai_enabled {
            return;
        }
        let Some(client) = ai::AiClient::from_env() else {
            log::warn!("AI palette: no provider configured");
            self.show_toast("No AI provider is configured.");
            return;
        };
        let cwd = self
            .active_cwd()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "<unknown>".to_string());
        let (system, user) = ai::build_nl_to_cmd_prompt(&query, &cwd);
        let sender_clone = sender.clone();

        // The worker and GLib timer each retain their own cancellation token,
        // so the callback remains live after this temporary handle is dropped.
        // Avoid `mem::forget`: every palette request used to leak one token.
        let _request = ai::ask(client, system, user, move |result| match result {
            Ok(response) => match command_for_review(&response) {
                Ok(command) => {
                    if let Some(reason) = agent::is_dangerous(&command) {
                        sender_clone.input(AppMsg::Toast(format!(
                            "AI suggested a potentially destructive command ({reason}). Review it carefully before running."
                        )));
                    }
                    sender_clone.input(AppMsg::PaletteTypeCommand(command));
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

    /// Open the session-level AI panel with the configured history source.
    pub(crate) fn show_ai_session_panel(&self) {
        if !self.config.borrow().ai_enabled {
            return;
        }
        self.ai_panel.emit(dialogs::ai_panel::AiPanelMsg::Open(
            self.config.borrow().command_history_path.clone(),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::{command_for_review, strip_code_fences};

    #[test]
    fn strips_a_wrapping_language_fence() {
        assert_eq!(strip_code_fences("```bash\nprintf 'ok'\n```"), "printf 'ok'");
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
}
