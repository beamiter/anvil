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
        initial_context: Option<ai::BlockContext>,
    },
    Ask,
    Clear,
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
}

impl ConversationState {
    fn is_busy(&self) -> bool {
        self.active_epoch.is_some()
    }

    fn begin(&mut self, user: String) -> (u64, Vec<ai::Turn>) {
        self.epoch = self.epoch.wrapping_add(1);
        self.active_epoch = Some(self.epoch);
        self.history.push(ai::Turn {
            role: ai::Role::User,
            text: user,
        });
        (self.epoch, self.history.clone())
    }

    fn complete_success(&mut self, epoch: u64, answer: String) -> bool {
        if self.active_epoch != Some(epoch) {
            return false;
        }
        self.active_epoch = None;
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
        self.history.clear();
    }
}

pub(crate) struct AiPanelModel {
    parent: adw::ApplicationWindow,
    history_path: Option<String>,
    client: Option<ai::AiClient>,
    in_flight: Option<ai::AiHandle>,
    pending_block_context: Option<ai::BlockContext>,
    conversation_system: Option<String>,
    conversation: ConversationState,
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
            in_flight: None,
            pending_block_context: None,
            conversation_system: None,
            conversation: ConversationState::default(),
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
                initial_context,
            } => {
                self.cancel();
                self.conversation.clear();
                self.history_path = history_path;
                self.client = Some(client);
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
                self.in_flight = Some(ai::ask_turns(client, system, history, move |result| {
                    sender.input(AiPanelMsg::Result { epoch, result });
                }));
            }
            AiPanelMsg::Clear => {
                self.cancel();
                self.conversation.clear();
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
            AiPanelMsg::Result { epoch, result } => match result {
                Ok(answer) => {
                    if !self
                        .conversation
                        .complete_success(epoch, answer.trim().to_string())
                    {
                        return;
                    }
                    self.in_flight = None;
                    widgets.spinner.stop();
                    widgets.spinner.set_visible(false);
                    widgets.ask_button.set_sensitive(true);
                    widgets.status.set_label("");
                    append_transcript(&widgets.answer, "Assistant", answer.trim());
                }
                Err(error) => {
                    if !self.conversation.complete_error(epoch) {
                        return;
                    }
                    self.in_flight = None;
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
    fn clear_invalidates_a_late_response() {
        let mut state = ConversationState::default();
        let (epoch, _) = state.begin("question".into());
        state.clear();
        assert!(!state.complete_success(epoch, "late".into()));
        assert!(state.history.is_empty());
        assert!(!state.is_busy());
    }
}
