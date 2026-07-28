//! Relm4 component for session-level AI questions.

use relm4::adw;
use relm4::adw::prelude::*;
use relm4::gtk;
use relm4::prelude::*;

use crate::{ai, palette};

#[derive(Debug)]
pub(crate) enum AiPanelMsg {
    Open {
        history_path: Option<String>,
        client: ai::AiClient,
        /// Stream replies incrementally (`ai_stream`); off falls back to the
        /// single-callback blocking transport.
        stream: bool,
        initial_context: Option<ai::BlockContext>,
    },
    Ask,
    Clear,
    /// One streamed assistant text fragment for the request begun at `epoch`.
    Delta {
        epoch: u64,
        text: String,
    },
    Result {
        epoch: u64,
        result: Result<String, String>,
    },
    Closed,
}

#[derive(Default)]
struct ConversationState {
    history: Vec<ai::Turn>,
    epoch: u64,
    active_epoch: Option<u64>,
    /// Streamed fragments shown so far for the active request. The complete
    /// text returned on success — never this accumulation — is what enters
    /// `history`: it can carry a trailing token-limit advisory that never
    /// arrived as a delta, and replacing the shown partial with it heals any
    /// dropped fragment.
    partial: String,
}

impl ConversationState {
    fn is_busy(&self) -> bool {
        self.active_epoch.is_some()
    }

    fn begin(&mut self, user: String) -> (u64, Vec<ai::Turn>) {
        self.epoch = self.epoch.wrapping_add(1);
        self.active_epoch = Some(self.epoch);
        self.partial.clear();
        self.history.push(ai::Turn {
            role: ai::Role::User,
            text: user,
        });
        (self.epoch, self.history.clone())
    }

    /// Accumulate one streamed fragment; stale-epoch fragments are dropped.
    fn push_delta(&mut self, epoch: u64, fragment: &str) -> bool {
        if self.active_epoch != Some(epoch) {
            return false;
        }
        self.partial.push_str(fragment);
        true
    }

    /// The streamed text shown for the active request; empty for blocking
    /// requests and once a request settles.
    fn shown_partial(&self) -> &str {
        &self.partial
    }

    fn complete_success(&mut self, epoch: u64, answer: String) -> bool {
        if self.active_epoch != Some(epoch) {
            return false;
        }
        self.active_epoch = None;
        self.partial.clear();
        self.history.push(ai::Turn {
            role: ai::Role::Assistant,
            text: answer,
        });
        true
    }

    fn complete_error(&mut self, epoch: u64) -> bool {
        if self.active_epoch != Some(epoch) {
            return false;
        }
        self.active_epoch = None;
        self.partial.clear();
        if self
            .history
            .last()
            .is_some_and(|turn| turn.role == ai::Role::User)
        {
            self.history.pop();
        }
        true
    }

    fn cancel_active(&mut self) {
        if let Some(epoch) = self.active_epoch {
            self.complete_error(epoch);
        }
        self.epoch = self.epoch.wrapping_add(1);
    }

    fn clear(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.active_epoch = None;
        self.partial.clear();
        self.history.clear();
    }
}

pub(crate) struct AiPanelModel {
    parent: adw::ApplicationWindow,
    history_path: Option<String>,
    client: Option<ai::AiClient>,
    stream: bool,
    in_flight: Option<ai::AiHandle>,
    pending_block_context: Option<ai::BlockContext>,
    conversation_system: Option<String>,
    conversation: ConversationState,
    /// Marks where the streamed assistant body begins in the transcript so
    /// the final complete text can replace it in place. Present only while a
    /// streamed request has shown at least one fragment.
    stream_anchor: Option<gtk::TextMark>,
}

#[relm4::component(pub(crate))]
impl Component for AiPanelModel {
    type Init = adw::ApplicationWindow;
    type Input = AiPanelMsg;
    type Output = ();
    type CommandOutput = ();

    view! {
        root = adw::Dialog {
            set_title: "Ask AI",
            set_content_width: 640,
            set_content_height: 520,
            connect_closed => AiPanelMsg::Closed,

            #[wrap(Some)]
            set_child = &adw::ToolbarView {
                add_top_bar = &adw::HeaderBar {},

                #[wrap(Some)]
                set_content = &gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_spacing: 8,
                    set_margin_all: 12,

                    gtk::Label {
                        set_label: "Your question:",
                        set_halign: gtk::Align::Start,
                        add_css_class: "dim-label",
                    },

                    gtk::ScrolledWindow {
                        set_hexpand: true,
                        set_min_content_height: 80,

                        #[name(entry)]
                        gtk::TextView {
                            set_wrap_mode: gtk::WrapMode::WordChar,
                            set_height_request: 80,
                            add_css_class: "ai-panel-entry",

                            add_controller = gtk::EventControllerKey {
                                connect_key_pressed[sender] => move |_, key, _, state| {
                                    use gtk::gdk::{Key, ModifierType};
                                    if matches!(key, Key::Return | Key::KP_Enter)
                                        && state.contains(ModifierType::CONTROL_MASK)
                                    {
                                        sender.input(AiPanelMsg::Ask);
                                        gtk::glib::Propagation::Stop
                                    } else {
                                        gtk::glib::Propagation::Proceed
                                    }
                                },
                            },
                        },
                    },

                    #[name(attach_context)]
                    gtk::CheckButton {
                        set_label: Some("Include recent shell context"),
                        set_active: true,
                    },

                    gtk::Box {
                        set_orientation: gtk::Orientation::Horizontal,
                        set_spacing: 6,
                        set_halign: gtk::Align::End,

                        #[name(status)]
                        gtk::Label {
                            set_halign: gtk::Align::Start,
                            set_hexpand: true,
                            add_css_class: "dim-label",
                        },

                        gtk::Button {
                            set_label: "Clear",
                            add_css_class: "flat",
                            connect_clicked => AiPanelMsg::Clear,
                        },

                        #[name(ask_button)]
                        gtk::Button {
                            set_label: "Ask",
                            add_css_class: "suggested-action",
                            connect_clicked => AiPanelMsg::Ask,
                        },
                    },

                    #[name(spinner)]
                    gtk::Spinner {
                        set_visible: false,
                    },

                    gtk::ScrolledWindow {
                        set_hexpand: true,
                        set_vexpand: true,

                        #[name(answer)]
                        gtk::TextView {
                            set_editable: false,
                            set_cursor_visible: false,
                            set_wrap_mode: gtk::WrapMode::WordChar,
                            add_css_class: "ai-explain-body",
                        },
                    },
                },
            },
        }
    }

    fn init(
        parent: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let model = Self {
            parent,
            history_path: None,
            client: None,
            stream: true,
            in_flight: None,
            pending_block_context: None,
            conversation_system: None,
            conversation: ConversationState::default(),
            stream_anchor: None,
        };
        let widgets = view_output!();
        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        msg: Self::Input,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        match msg {
            AiPanelMsg::Open {
                history_path,
                client,
                stream,
                initial_context,
            } => {
                self.cancel();
                self.conversation.clear();
                self.drop_stream_anchor(&widgets.answer);
                self.history_path = history_path;
                self.client = Some(client);
                self.stream = stream;
                self.pending_block_context = initial_context;
                self.conversation_system = None;
                widgets.status.set_label("");
                widgets.answer.buffer().set_text("");
                widgets.entry.buffer().set_text("");
                root.present(Some(&self.parent));
                widgets.entry.grab_focus();
                if let Some(context) = self.pending_block_context.as_ref() {
                    widgets.entry.buffer().set_text(if context.exit_code == 0 {
                        "Explain what this command does and what its output means."
                    } else {
                        "This command failed. Diagnose the error and suggest a fix."
                    });
                    sender.input(AiPanelMsg::Ask);
                }
            }
            AiPanelMsg::Ask => {
                if self.conversation.is_busy() {
                    return;
                }
                let buffer = widgets.entry.buffer();
                let (start, end) = buffer.bounds();
                let question = buffer.text(&start, &end, true);
                let question = question.trim();
                if question.is_empty() {
                    widgets.status.set_label("(question is empty)");
                    return;
                }
                let Some(client) = self.client.clone() else {
                    widgets.status.set_label("No AI provider is configured.");
                    return;
                };
                let (new_system, api_user) =
                    if let Some(context) = self.pending_block_context.take() {
                        ai::build_block_chat_prompt(question, &context)
                    } else {
                        let context = if widgets.attach_context.is_active() {
                            self.recent_context()
                        } else {
                            None
                        };
                        ai::build_session_prompt(question, context.as_deref())
                    };
                let system = self.conversation_system.get_or_insert(new_system).clone();
                let visible_question = question.to_string();
                let (epoch, history) = self.conversation.begin(api_user);
                append_transcript(&widgets.answer, "You", &visible_question);
                widgets.entry.buffer().set_text("");
                widgets
                    .status
                    .set_label(&format!("Asking {} …", client.display_name()));
                widgets.spinner.set_visible(true);
                widgets.spinner.start();
                widgets.ask_button.set_sensitive(false);
                self.in_flight = Some(if self.stream {
                    let delta_sender = sender.clone();
                    ai::ask_turns_streaming(
                        client,
                        system,
                        history,
                        move |text| delta_sender.input(AiPanelMsg::Delta { epoch, text }),
                        move |result| sender.input(AiPanelMsg::Result { epoch, result }),
                    )
                } else {
                    ai::ask_turns(client, system, history, move |result| {
                        sender.input(AiPanelMsg::Result { epoch, result });
                    })
                });
            }
            AiPanelMsg::Clear => {
                self.cancel();
                self.conversation.clear();
                self.drop_stream_anchor(&widgets.answer);
                self.pending_block_context = None;
                self.conversation_system = None;
                widgets.entry.buffer().set_text("");
                widgets.answer.buffer().set_text("");
                widgets.status.set_label("");
                widgets.spinner.stop();
                widgets.spinner.set_visible(false);
                widgets.ask_button.set_sensitive(true);
                widgets.entry.grab_focus();
            }
            AiPanelMsg::Delta { epoch, text } => {
                if !self.conversation.push_delta(epoch, &text) {
                    return;
                }
                let buffer = widgets.answer.buffer();
                if self.stream_anchor.is_none() {
                    // Lazily open the assistant section on the first fragment
                    // so an early failure never leaves an empty heading, and
                    // anchor the body start (left gravity) so the complete
                    // text can replace the partial in place.
                    let mut end = buffer.end_iter();
                    if buffer.char_count() > 0 {
                        buffer.insert(&mut end, "\n\n");
                    }
                    buffer.insert(&mut end, "Assistant\n");
                    let end = buffer.end_iter();
                    self.stream_anchor = Some(buffer.create_mark(None, &end, true));
                }
                let mut end = buffer.end_iter();
                buffer.insert(&mut end, &text);
                scroll_to_end(&widgets.answer);
            }
            AiPanelMsg::Result { epoch, result } => match result {
                Ok(answer) => {
                    let answer = answer.trim().to_string();
                    let partial_matches = self.conversation.shown_partial() == answer;
                    if !self.conversation.complete_success(epoch, answer.clone()) {
                        return;
                    }
                    self.in_flight = None;
                    widgets.spinner.stop();
                    widgets.spinner.set_visible(false);
                    widgets.ask_button.set_sensitive(true);
                    widgets.status.set_label("");
                    match self.stream_anchor.take() {
                        Some(anchor) => {
                            // The returned complete text is the single source
                            // of truth; swap it in unless the streamed
                            // fragments already add up to exactly the same
                            // bytes.
                            if !partial_matches {
                                let buffer = widgets.answer.buffer();
                                let mut start = buffer.iter_at_mark(&anchor);
                                let mut end = buffer.end_iter();
                                buffer.delete(&mut start, &mut end);
                                let mut end = buffer.end_iter();
                                buffer.insert(&mut end, &answer);
                                scroll_to_end(&widgets.answer);
                            }
                            widgets.answer.buffer().delete_mark(&anchor);
                        }
                        None => append_transcript(&widgets.answer, "Assistant", &answer),
                    }
                }
                Err(error) => {
                    if !self.conversation.complete_error(epoch) {
                        return;
                    }
                    self.in_flight = None;
                    // A mid-stream failure keeps the fragments already shown;
                    // only the replace anchor is released.
                    if let Some(anchor) = self.stream_anchor.take() {
                        widgets.answer.buffer().delete_mark(&anchor);
                    }
                    widgets.spinner.stop();
                    widgets.spinner.set_visible(false);
                    widgets.ask_button.set_sensitive(true);
                    widgets.status.set_label(&format!("AI error: {error}"));
                }
            },
            AiPanelMsg::Closed => self.cancel(),
        }
    }
}

impl AiPanelModel {
    fn cancel(&mut self) {
        if let Some(handle) = self.in_flight.take() {
            handle.cancel();
        }
        self.conversation.cancel_active();
    }

    /// Release the in-place replace anchor without touching the shown text.
    fn drop_stream_anchor(&mut self, answer: &gtk::TextView) {
        if let Some(anchor) = self.stream_anchor.take() {
            answer.buffer().delete_mark(&anchor);
        }
    }

    fn recent_context(&self) -> Option<String> {
        let path = self.history_path.as_deref()?;
        let items = palette::read_history(std::path::Path::new(path), 5);
        if items.is_empty() {
            return None;
        }
        let mut context = String::new();
        for item in items.iter().rev() {
            context.push_str(&format!("$ {} (exit {})\n", item.command, item.exit_code));
        }
        Some(context)
    }
}

fn append_transcript(view: &gtk::TextView, label: &str, body: &str) {
    let buffer = view.buffer();
    let mut end = buffer.end_iter();
    if buffer.char_count() > 0 {
        buffer.insert(&mut end, "\n\n");
    }
    buffer.insert(&mut end, label);
    buffer.insert(&mut end, "\n");
    buffer.insert(&mut end, body);
    scroll_to_end(view);
}

fn scroll_to_end(view: &gtk::TextView) {
    let view = view.clone();
    gtk::glib::idle_add_local_once(move || {
        let mut end = view.buffer().end_iter();
        view.scroll_to_iter(&mut end, 0.0, false, 0.0, 1.0);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_turns_alternate_and_errors_do_not_poison_history() {
        let mut state = ConversationState::default();
        let (one, _) = state.begin("one".into());
        assert!(state.complete_success(one, "answer".into()));
        let (two, sent) = state.begin("two".into());
        assert_eq!(sent.len(), 3);
        assert_eq!(sent[0].role, ai::Role::User);
        assert_eq!(sent[1].role, ai::Role::Assistant);
        assert_eq!(sent[2].role, ai::Role::User);
        assert!(state.complete_error(two));
        assert_eq!(state.history.len(), 2);
    }

    #[test]
    fn streamed_fragments_accumulate_only_for_the_active_request() {
        let mut state = ConversationState::default();
        let (epoch, _) = state.begin("question".into());
        assert!(state.push_delta(epoch, "Hel"));
        assert!(state.push_delta(epoch, "lo"));
        assert!(!state.push_delta(epoch.wrapping_add(1), "stale"));
        assert_eq!(state.shown_partial(), "Hello");
    }

    #[test]
    fn final_text_replaces_the_streamed_partial_in_history() {
        let mut state = ConversationState::default();
        let (epoch, _) = state.begin("question".into());
        assert!(state.push_delta(epoch, "Hello"));
        // The complete text carries a trailing advisory that never streamed;
        // it — not the accumulated fragments — must be recorded.
        assert!(state.complete_success(epoch, "Hello\n\n[reply truncated]".into()));
        assert_eq!(state.shown_partial(), "");
        let recorded = state.history.last().unwrap();
        assert_eq!(recorded.role, ai::Role::Assistant);
        assert_eq!(recorded.text, "Hello\n\n[reply truncated]");
    }

    #[test]
    fn a_failed_stream_resets_partial_and_history_like_the_blocking_path() {
        let mut state = ConversationState::default();
        let (epoch, _) = state.begin("question".into());
        assert!(state.push_delta(epoch, "partial answer"));
        assert!(state.complete_error(epoch));
        assert_eq!(state.shown_partial(), "");
        assert!(state.history.is_empty());
        // A retry starts from the same clean state the blocking path leaves
        // behind and streams under its own epoch.
        let (retry, sent) = state.begin("question".into());
        assert_eq!(sent.len(), 1);
        assert!(state.push_delta(retry, "again"));
        assert_eq!(state.shown_partial(), "again");
    }

    #[test]
    fn clear_invalidates_a_late_response() {
        let mut state = ConversationState::default();
        let (epoch, _) = state.begin("question".into());
        state.clear();
        assert!(!state.complete_success(epoch, "late".into()));
        assert!(state.history.is_empty());
        assert!(!state.is_busy());
    }
}
