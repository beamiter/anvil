//! Natural-language command suggestion and session-level AI panel operations.

use super::*;
use crate::command_review::{CommandReviewCard, CommandReviewSpec, ReviewPresentation};

pub(crate) struct CommandSuggestionSession {
    generation: u64,
    request_id: u64,
    pane_id: u64,
    request: String,
    cwd: String,
    shell: String,
    block_context: Option<ai::BlockContext>,
    provider: String,
    card: gtk::Widget,
    review_box: gtk::Box,
    status: gtk::Label,
    spinner: gtk::Spinner,
    stop: gtk::Button,
    retry: gtk::Button,
    review: Option<CommandReviewCard>,
    in_flight: Option<ai::AiHandle>,
    busy: bool,
}

fn compact_one_line(text: &str, max_chars: usize) -> String {
    let safe = crate::review_input::safe_inline_display(text, 16 * 1024);
    let collapsed = safe.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{preview}…")
    } else if preview.is_empty() {
        "(empty)".to_string()
    } else {
        preview
    }
}

fn clear_review(session: &mut CommandSuggestionSession) {
    while let Some(child) = session.review_box.first_child() {
        session.review_box.remove(&child);
    }
    session.review_box.set_visible(false);
    session.review = None;
}

fn suggestion_reply_is_current(
    session_generation: u64,
    session_request_id: u64,
    busy: bool,
    generation: u64,
    request_id: u64,
) -> bool {
    busy && session_generation == generation && session_request_id == request_id
}

fn set_suggestion_status(
    session: &CommandSuggestionSession,
    message: &str,
    active: bool,
    error: bool,
) {
    session
        .status
        .set_text(&crate::review_input::safe_inline_display(
            message,
            16 * 1024,
        ));
    if error {
        session.status.add_css_class("error");
    } else {
        session.status.remove_css_class("error");
    }
    if active {
        session.spinner.start();
    } else {
        session.spinner.stop();
    }
    session.stop.set_visible(active);
    session.stop.set_sensitive(active);
    session.retry.set_visible(!active && error);
    session.retry.set_sensitive(!active && error);
}

impl AppModel {
    /// Open a pane-bound, review-only suggestion card immediately. The request
    /// handle lives in `command_suggestion` until reply/Stop/Dismiss, so Drop
    /// cancellation can no longer abort every `?` request at function return.
    pub(crate) fn handle_palette_ask_ai(&self, query: String, sender: &ComponentSender<AppModel>) {
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
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let Some(pane) = tab.panes.get(tab.active_pane) else {
            return;
        };
        if !matches!(pane.mode, TerminalMode::Block) {
            self.show_toast("AI command suggestions require an active Block pane.");
            return;
        }
        let pane_id = pane.id;
        let cwd = pane.cwd.clone().unwrap_or_else(|| ".".to_string());
        let shell = self
            .shell_argv
            .first()
            .cloned()
            .unwrap_or_else(|| "sh".to_string());
        let block_context = pane.terminal.selected_block_context(80);
        let compact = self.config.borrow().block_compact;
        let provider = client.display_name();
        self.close_command_suggestion();

        let generation = self
            .command_suggestion_generation
            .get()
            .checked_add(1)
            .unwrap_or(1);
        self.command_suggestion_generation.set(generation);

        let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
        outer.add_css_class("block-finished");
        outer.add_css_class("block-assistant");
        outer.add_css_class("command-suggestion");
        outer.set_hexpand(true);
        outer.set_vexpand(false);
        if compact {
            outer.add_css_class("block-compact");
            outer.set_margin_top(1);
            outer.set_margin_bottom(1);
            outer.set_margin_start(4);
            outer.set_margin_end(4);
        } else {
            outer.set_margin_top(4);
            outer.set_margin_bottom(4);
            outer.set_margin_start(8);
            outer.set_margin_end(8);
        }

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        header.add_css_class("block-header");
        header.set_margin_start(if compact { 8 } else { 12 });
        header.set_margin_end(if compact { 6 } else { 8 });
        header.set_margin_top(if compact { 3 } else { 6 });
        header.set_margin_bottom(if compact { 1 } else { 2 });
        let icon = gtk::Image::from_icon_name("dialog-information-symbolic");
        icon.add_css_class("assistant-card-icon");
        header.append(&icon);
        let title = gtk::Label::new(Some("AI command suggestion"));
        title.add_css_class("assistant-card-title");
        title.set_xalign(0.0);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        header.append(&title);
        let cwd_display = crate::review_input::safe_inline_display(&cwd, 4 * 1024);
        let binding = gtk::Label::new(Some(&format!("{cwd_display} · review only")));
        binding.add_css_class("assistant-card-badge");
        binding.set_hexpand(true);
        binding.set_halign(gtk::Align::End);
        binding.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        binding.set_tooltip_text(Some(&cwd_display));
        header.append(&binding);
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("flat");
        close.set_focusable(false);
        close.set_tooltip_text(Some("Stop and dismiss this suggestion (Esc)"));
        header.append(&close);
        outer.append(&header);

        let body = gtk::Box::new(gtk::Orientation::Vertical, 7);
        body.set_margin_start(if compact { 8 } else { 12 });
        body.set_margin_end(if compact { 8 } else { 12 });
        body.set_margin_top(2);
        body.set_margin_bottom(if compact { 7 } else { 11 });
        let request_label =
            gtk::Label::new(Some(&format!("Request: {}", compact_one_line(&query, 180))));
        request_label.add_css_class("command-review-description");
        request_label.set_xalign(0.0);
        request_label.set_hexpand(true);
        request_label.set_wrap(true);
        request_label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        request_label.set_selectable(true);
        body.append(&request_label);
        if let Some(context) = block_context.as_ref() {
            let context_label = gtk::Label::new(Some(&format!(
                "Selected Block context · exit {}{} · {}",
                context.exit_code,
                if context.truncated {
                    " · output truncated"
                } else {
                    ""
                },
                compact_one_line(&context.cmd, 72)
            )));
            context_label.add_css_class("assistant-context-chip");
            context_label.set_xalign(0.0);
            context_label.set_hexpand(true);
            context_label.set_wrap(true);
            context_label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            context_label.set_tooltip_text(Some(
                "Attached as bounded, untrusted command/output context for this request",
            ));
            body.append(&context_label);
        }

        let spinner = gtk::Spinner::new();
        let status = gtk::Label::new(Some("Preparing command suggestion…"));
        status.add_css_class("assistant-status");
        status.set_xalign(0.0);
        status.set_wrap(true);
        status.set_hexpand(true);
        status.set_accessible_role(gtk::AccessibleRole::Status);
        let status_row = gtk::Box::new(gtk::Orientation::Horizontal, 7);
        status_row.add_css_class("assistant-status-row");
        status_row.append(&spinner);
        status_row.append(&status);
        body.append(&status_row);
        let retry = gtk::Button::with_label("Retry");
        retry.add_css_class("command-review-secondary");
        retry.set_visible(false);
        let stop = gtk::Button::with_label("Stop");
        stop.add_css_class("destructive-action");
        stop.set_visible(false);
        // Stop and Retry are mutually exclusive, so a plain row is both
        // narrower and more reliable than wrapping initially hidden children.
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        controls.set_hexpand(true);
        controls.set_halign(gtk::Align::End);
        controls.append(&retry);
        controls.append(&stop);
        body.append(&controls);
        let review_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        review_box.set_visible(false);
        body.append(&review_box);
        let hint = gtk::Label::new(Some(
            "Enter uses the labelled action · generated commands never run automatically",
        ));
        hint.add_css_class("agent-input-hint");
        hint.set_xalign(0.0);
        hint.set_wrap(true);
        hint.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        body.append(&hint);
        outer.append(&body);

        let card: gtk::Widget = outer.clone().upcast();
        *self.command_suggestion.borrow_mut() = Some(CommandSuggestionSession {
            generation,
            request_id: 0,
            pane_id,
            request: query,
            cwd,
            shell,
            block_context,
            provider,
            card: card.clone(),
            review_box,
            status,
            spinner,
            stop: stop.clone(),
            retry: retry.clone(),
            review: None,
            in_flight: None,
            busy: false,
        });

        {
            let sender = sender.clone();
            close.connect_clicked(move |_| {
                sender.input(AppMsg::PaletteSuggestionDismiss(generation));
            });
        }
        {
            let sender = sender.clone();
            stop.connect_clicked(move |_| {
                sender.input(AppMsg::PaletteSuggestionStop(generation));
            });
        }
        {
            let sender = sender.clone();
            retry.connect_clicked(move |_| {
                sender.input(AppMsg::PaletteSuggestionRetry(generation));
            });
        }
        {
            let sender = sender.clone();
            let keys = gtk::EventControllerKey::new();
            keys.set_propagation_phase(gtk::PropagationPhase::Capture);
            keys.connect_key_pressed(move |_, key, _, _| {
                if key == gtk::gdk::Key::Escape {
                    sender.input(AppMsg::PaletteSuggestionDismiss(generation));
                    gtk::glib::Propagation::Stop
                } else {
                    gtk::glib::Propagation::Proceed
                }
            });
            outer.add_controller(keys);
        }

        if let Some(terminal) = self.terminal_for_pane(pane_id) {
            terminal.insert_inline_notice(&card);
        }
        self.start_command_suggestion(generation, sender);
    }

    pub(crate) fn start_command_suggestion(
        &self,
        generation: u64,
        sender: &ComponentSender<AppModel>,
    ) {
        let client = match ai::client_from_config(&self.config.borrow()) {
            Ok(client) => client,
            Err(error) => {
                if let Some(session) = self
                    .command_suggestion
                    .borrow()
                    .as_ref()
                    .filter(|session| session.generation == generation)
                {
                    set_suggestion_status(session, &error, false, true);
                }
                return;
            }
        };
        let request = {
            let mut slot = self.command_suggestion.borrow_mut();
            let Some(session) = slot
                .as_mut()
                .filter(|session| session.generation == generation && !session.busy)
            else {
                return;
            };
            clear_review(session);
            session.busy = true;
            session.request_id = session.request_id.wrapping_add(1).max(1);
            set_suggestion_status(
                session,
                &format!("Drafting with {} for this Block pane…", session.provider),
                true,
                false,
            );
            (
                session.request_id,
                session.request.clone(),
                session.cwd.clone(),
                session.shell.clone(),
                session.block_context.clone(),
            )
        };
        let sender = sender.clone();
        let handle = ai::generate_command(
            client,
            request.1,
            request.2,
            request.3,
            request.4,
            move |reply| {
                sender.input(AppMsg::PaletteSuggestionReply {
                    generation,
                    request_id: request.0,
                    reply,
                });
            },
        );
        let mut handle = Some(handle);
        if let Some(session) = self
            .command_suggestion
            .borrow_mut()
            .as_mut()
            .filter(|session| {
                suggestion_reply_is_current(
                    session.generation,
                    session.request_id,
                    session.busy,
                    generation,
                    request.0,
                )
            })
        {
            session.in_flight = handle.take();
        }
        drop(handle);
    }

    pub(crate) fn command_suggestion_reply(
        &self,
        generation: u64,
        request_id: u64,
        reply: Result<String, String>,
        sender: &ComponentSender<AppModel>,
    ) {
        let compact = self.config.borrow().block_compact;
        let mut slot = self.command_suggestion.borrow_mut();
        let Some(session) = slot.as_mut().filter(|session| {
            suggestion_reply_is_current(
                session.generation,
                session.request_id,
                session.busy,
                generation,
                request_id,
            )
        }) else {
            return;
        };
        session.in_flight.take();
        session.busy = false;
        let command = match reply {
            Ok(command) => command,
            Err(error) => {
                set_suggestion_status(
                    session,
                    &format!("Command suggestion failed: {error}"),
                    false,
                    true,
                );
                return;
            }
        };
        set_suggestion_status(
            session,
            "Review the proposal below. Nothing has been inserted or run.",
            false,
            false,
        );
        let review = CommandReviewCard::new(CommandReviewSpec {
            presentation: ReviewPresentation::Embedded,
            compact,
            icon_name: "dialog-information-symbolic",
            title: "Command proposal".to_string(),
            badge: session.provider.clone(),
            description: format!("Generated for: {}", compact_one_line(&session.request, 140)),
            command,
            primary_label: "Insert for review".to_string(),
            primary_executes: false,
            auxiliary_label: None,
            secondary_label: Some("Regenerate".to_string()),
            close_button: false,
        });
        {
            let sender = sender.clone();
            review.primary.connect_clicked(move |_| {
                sender.input(AppMsg::PaletteSuggestionInsert(generation));
            });
        }
        {
            let sender = sender.clone();
            review.entry.connect_activate(move |_| {
                sender.input(AppMsg::PaletteSuggestionInsert(generation));
            });
        }
        if let Some(regenerate) = review.secondary.as_ref() {
            let sender = sender.clone();
            regenerate.connect_clicked(move |_| {
                sender.input(AppMsg::PaletteSuggestionRetry(generation));
            });
        }
        session.review_box.set_visible(true);
        session.review_box.append(&review.root);
        review.focus();
        session.review = Some(review);
    }

    pub(crate) fn stop_command_suggestion(&self, generation: u64) {
        let mut slot = self.command_suggestion.borrow_mut();
        let Some(session) = slot
            .as_mut()
            .filter(|session| session.generation == generation && session.busy)
        else {
            return;
        };
        session.in_flight.take();
        session.busy = false;
        set_suggestion_status(
            session,
            "Suggestion request stopped. Retry when ready.",
            false,
            true,
        );
    }

    pub(crate) fn insert_command_suggestion(&self, generation: u64) {
        let (pane_id, command) = {
            let slot = self.command_suggestion.borrow();
            let Some(session) = slot
                .as_ref()
                .filter(|session| session.generation == generation)
            else {
                return;
            };
            let Some(review) = session.review.as_ref() else {
                return;
            };
            let command = match review.validated_command() {
                Ok(command) => command,
                Err(error) => {
                    review.show_error(&format!("Cannot insert: {error}"));
                    return;
                }
            };
            (session.pane_id, command)
        };
        if !self.ai_command_target_is_ready(pane_id) {
            if let Some(review) = self
                .command_suggestion
                .borrow()
                .as_ref()
                .and_then(|session| session.review.as_ref())
            {
                let message = self
                    .terminal_for_pane(pane_id)
                    .map(|terminal| terminal.command_prompt_status().blocked_message())
                    .unwrap_or("The target Block pane no longer exists.");
                review.show_error(message);
            }
            return;
        }
        if self.insert_review_text_into_pane(pane_id, &command) {
            self.close_command_suggestion_generation(generation);
        }
    }

    pub(crate) fn pin_command_suggestion(&self, pane_id: u64) {
        let slot = self.command_suggestion.borrow();
        let Some(session) = slot.as_ref().filter(|session| session.pane_id == pane_id) else {
            return;
        };
        if let Some(terminal) = self.terminal_for_pane(pane_id) {
            terminal.insert_inline_notice(&session.card);
        }
    }

    pub(crate) fn close_command_suggestion_generation(&self, generation: u64) {
        let matches = self
            .command_suggestion
            .borrow()
            .as_ref()
            .is_some_and(|session| session.generation == generation);
        if matches {
            self.close_command_suggestion();
        }
    }

    pub(crate) fn close_command_suggestion_for_pane(&self, pane_id: u64) {
        let matches = self
            .command_suggestion
            .borrow()
            .as_ref()
            .is_some_and(|session| session.pane_id == pane_id);
        if matches {
            self.close_command_suggestion();
        }
    }

    pub(crate) fn close_command_suggestion(&self) {
        let Some(mut session) = self.command_suggestion.borrow_mut().take() else {
            return;
        };
        session.in_flight.take();
        session.spinner.stop();
        if let Some(terminal) = self.terminal_for_pane(session.pane_id) {
            terminal.remove_inline_notice(&session.card);
        }
    }

    fn ai_command_target_is_ready(&self, pane_id: u64) -> bool {
        self.terminal_for_pane(pane_id)
            .is_some_and(|terminal| terminal.command_prompt_status().is_ready())
    }

    fn terminal_for_pane(&self, pane_id: u64) -> Option<&TermCtl> {
        let (tab_index, pane_index) = self.find_pane(pane_id)?;
        Some(&self.tabs[tab_index].panes[pane_index].terminal)
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
        if !self.config.borrow().ai_enabled {
            self.show_toast("AI features are disabled in Settings.");
            return;
        }
        // Visibility is a panel preference, not proof that provider
        // credentials are currently usable. This also matches startup, where
        // a restored panel stays visible and explains provider errors in
        // place instead of silently changing the saved preference.
        self.set_ai_panel_visible(true, true);
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
            redact_secrets: self.config.borrow().ai_redact_secrets,
            initial_context,
        });
    }

    /// Apply the combined right-side panel state: the shared stack is visible
    /// when either panel wants the slot, and the Tasks panel takes the page
    /// while it is open (the AI preference survives underneath).
    pub(crate) fn sync_side_panel(&self) {
        let tasks = self.tasks_panel_visible.get();
        let chats = self.ai_panel_visible.get();
        self.side_stack.set_visible(tasks || chats);
        self.side_stack
            .set_visible_child_name(if tasks { "tasks" } else { "chats" });
    }

    pub(crate) fn set_ai_panel_visible(&self, visible: bool, persist: bool) {
        let visible = visible && !self.safe_mode && self.config.borrow().ai_enabled;
        if self.ai_panel_visible.get() == visible {
            if visible {
                self.restore_ai_panel_width();
            }
            self.sync_side_panel();
            return;
        }
        if !visible && self.ai_panel_visible.get() {
            let measured = self
                .ai_paned
                .width()
                .saturating_sub(self.ai_paned.position());
            if measured >= MIN_AI_PANEL_WIDTH as i32 {
                self.config.borrow_mut().ai_panel_width =
                    (measured as u32).clamp(MIN_AI_PANEL_WIDTH, MAX_AI_PANEL_WIDTH);
            }
        }
        self.ai_panel_visible.set(visible);
        self.config.borrow_mut().ai_panel_visible = visible;
        self.sync_side_panel();
        if visible {
            self.restore_ai_panel_width();
        }
        if persist {
            self.persist_config();
        }
    }

    pub(crate) fn restore_ai_panel_width(&self) {
        if !self.ai_panel_visible.get() {
            return;
        }
        let paned = self.ai_paned.clone();
        let requested = self.config.borrow().ai_panel_width;
        if let Some(position) = restored_ai_panel_position(paned.width(), requested) {
            paned.set_position(position);
        }
        gtk::glib::idle_add_local_once(move || {
            if let Some(position) = restored_ai_panel_position(paned.width(), requested) {
                paned.set_position(position);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ai_panel_width_from_geometry, compact_one_line, restored_ai_panel_position,
        suggestion_reply_is_current,
    };

    #[test]
    fn compact_preview_collapses_and_bounds_untrusted_text() {
        assert_eq!(compact_one_line("  list\n files  ", 20), "list� files");
        assert_eq!(compact_one_line("abcdefgh", 4), "abcd…");
        assert_eq!(compact_one_line("\u{202e}\u{fff0}\u{e0080}", 20), "���");
    }

    #[test]
    fn stopped_or_retried_request_cannot_publish_a_stale_reply() {
        assert!(suggestion_reply_is_current(4, 2, true, 4, 2));
        assert!(!suggestion_reply_is_current(4, 2, false, 4, 2));
        assert!(!suggestion_reply_is_current(4, 3, true, 4, 2));
        assert!(!suggestion_reply_is_current(5, 2, true, 4, 2));
    }

    #[test]
    fn ai_panel_width_restore_preserves_a_usable_terminal() {
        assert_eq!(restored_ai_panel_position(800, 360), Some(440));
        assert_eq!(restored_ai_panel_position(2_000, 1_800), Some(200));
        assert_eq!(restored_ai_panel_position(440, 360), None);
        assert_eq!(ai_panel_width_from_geometry(800, 440), Some(360));
        assert_eq!(ai_panel_width_from_geometry(2_000, 100), Some(1_200));
        assert_eq!(ai_panel_width_from_geometry(800, 800), None);
    }
}
