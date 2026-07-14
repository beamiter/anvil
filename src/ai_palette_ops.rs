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
        // Fire and forget — keeping the handle would just let us cancel, but
        // the palette has already closed by the time we get here, so there's
        // nothing user-visible to cancel against.
        let _h = ai::ask(client, system, user, move |result| match result {
            Ok(cmd) => {
                let cleaned = strip_code_fences(cmd.trim()).to_string();
                if !cleaned.is_empty() {
                    sender_clone.input(AppMsg::PaletteTypeCommand(cleaned));
                }
            }
            Err(e) => {
                log::warn!("AI palette request failed: {e}");
                sender_clone.input(AppMsg::Toast(format!("AI request failed: {e}")));
            }
        });
        std::mem::forget(_h);
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
