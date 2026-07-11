//! Relm4 component for session-level AI questions.

use relm4::adw;
use relm4::adw::prelude::*;
use relm4::gtk;
use relm4::prelude::*;

use crate::{ai, palette};

#[derive(Debug)]
pub(crate) enum AiPanelMsg {
    Open(Option<String>),
    Ask,
    Result(Result<String, String>),
    Closed,
}

pub(crate) struct AiPanelModel {
    parent: adw::ApplicationWindow,
    history_path: Option<String>,
    in_flight: Option<ai::AiHandle>,
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
            in_flight: None,
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
            AiPanelMsg::Open(history_path) => {
                self.cancel();
                self.history_path = history_path;
                widgets.status.set_label("");
                widgets.answer.buffer().set_text("");
                root.present(Some(&self.parent));
                widgets.entry.grab_focus();
            }
            AiPanelMsg::Ask => {
                let buffer = widgets.entry.buffer();
                let (start, end) = buffer.bounds();
                let question = buffer.text(&start, &end, true);
                let question = question.trim();
                if question.is_empty() {
                    widgets.status.set_label("(question is empty)");
                    return;
                }
                let Some(client) = ai::AiClient::from_env() else {
                    widgets.status.set_label(
                        "No AI provider configured. Set ANTHROPIC_API_KEY / OPENAI_API_KEY, \
                         or run `ollama serve`.",
                    );
                    return;
                };
                self.cancel();
                let context = if widgets.attach_context.is_active() {
                    self.recent_context()
                } else {
                    None
                };
                let (system, user) = ai::build_session_prompt(question, context.as_deref());
                widgets.answer.buffer().set_text("");
                widgets
                    .status
                    .set_label(&format!("Asking {} …", client.display_name()));
                widgets.spinner.set_visible(true);
                widgets.spinner.start();
                widgets.ask_button.set_sensitive(false);
                self.in_flight = Some(ai::ask(client, system, user, move |result| {
                    sender.input(AiPanelMsg::Result(result));
                }));
            }
            AiPanelMsg::Result(result) => {
                self.in_flight = None;
                widgets.spinner.stop();
                widgets.spinner.set_visible(false);
                widgets.ask_button.set_sensitive(true);
                match result {
                    Ok(answer) => {
                        widgets.status.set_label("");
                        widgets.answer.buffer().set_text(answer.trim());
                    }
                    Err(error) => widgets.status.set_label(&format!("AI error: {error}")),
                }
            }
            AiPanelMsg::Closed => self.cancel(),
        }
    }
}

impl AiPanelModel {
    fn cancel(&mut self) {
        if let Some(handle) = self.in_flight.take() {
            handle.cancel();
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
