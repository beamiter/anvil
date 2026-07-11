//! Relm4 component for find-in-terminal controls.

use relm4::gtk;
use relm4::gtk::prelude::*;
use relm4::prelude::*;

#[derive(Debug)]
pub(crate) enum SearchMsg {
    Toggle,
    Changed(String),
    Next,
    Previous,
    Close,
}

#[derive(Debug)]
pub(crate) enum SearchOutput {
    Changed(String),
    Next,
    Previous,
    Closed,
}

pub(crate) struct SearchModel;

#[relm4::component(pub(crate))]
impl Component for SearchModel {
    type Init = ();
    type Input = SearchMsg;
    type Output = SearchOutput;
    type CommandOutput = ();

    view! {
        root = gtk::SearchBar {
            set_search_mode: false,

            #[name(entry)]
            #[wrap(Some)]
            set_child = &gtk::SearchEntry {
                set_placeholder_text: Some("Find… (/regex/ for regex)"),
                set_hexpand: true,
                connect_search_changed[sender] => move |entry| {
                    sender.input(SearchMsg::Changed(entry.text().to_string()));
                },
                connect_activate => SearchMsg::Next,

                add_controller = gtk::EventControllerKey {
                    set_propagation_phase: gtk::PropagationPhase::Capture,
                    connect_key_pressed[sender] => move |_, key, _, state| {
                        use gtk::gdk::{Key, ModifierType};
                        let message = if key == Key::Escape {
                            Some(SearchMsg::Close)
                        } else if matches!(key, Key::Return | Key::KP_Enter)
                            && state.contains(ModifierType::SHIFT_MASK)
                        {
                            Some(SearchMsg::Previous)
                        } else {
                            None
                        };
                        if let Some(message) = message {
                            sender.input(message);
                            gtk::glib::Propagation::Stop
                        } else {
                            gtk::glib::Propagation::Proceed
                        }
                    },
                },
            },
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let model = Self;
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
            SearchMsg::Toggle => {
                let open = !root.is_search_mode();
                root.set_search_mode(open);
                if open {
                    widgets.entry.grab_focus();
                } else {
                    let _ = sender.output(SearchOutput::Closed);
                }
            }
            SearchMsg::Changed(query) => {
                let _ = sender.output(SearchOutput::Changed(query));
            }
            SearchMsg::Next => {
                let _ = sender.output(SearchOutput::Next);
            }
            SearchMsg::Previous => {
                let _ = sender.output(SearchOutput::Previous);
            }
            SearchMsg::Close => {
                root.set_search_mode(false);
                let _ = sender.output(SearchOutput::Closed);
            }
        }
    }
}
