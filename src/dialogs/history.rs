//! Relm4 component for the terminal-anchored history popover.

use relm4::adw;
use relm4::adw::prelude::*;
use relm4::gtk;
use relm4::prelude::*;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use crate::keybindings::{Action, KeybindingMap};
use crate::palette::{self, Accept, PaletteMode, Query};
use crate::workflows::Workflow;

pub(crate) struct HistoryInit {
    pub(crate) keybindings: Rc<RefCell<KeybindingMap>>,
    pub(crate) workflows: Rc<RefCell<Vec<Workflow>>>,
}

#[derive(Debug)]
pub(crate) enum HistoryMsg {
    Toggle {
        anchor: gtk::Widget,
        history_path: Option<PathBuf>,
    },
    Search(String),
    ActivateIndex(i32),
    Move(i32),
    AcceptSelected,
    Close,
    Closed,
}

#[derive(Debug)]
pub(crate) enum HistoryOutput {
    Action(Action),
    TypeCommand(String),
    AskAi(String),
    RunWorkflow(PathBuf),
}

pub(crate) struct HistoryModel {
    keybindings: Rc<RefCell<KeybindingMap>>,
    workflows: Rc<RefCell<Vec<Workflow>>>,
    history_path: Option<PathBuf>,
    query: String,
    accepts: Vec<Accept>,
}

#[relm4::component(pub(crate))]
impl Component for HistoryModel {
    type Init = HistoryInit;
    type Input = HistoryMsg;
    type Output = HistoryOutput;
    type CommandOutput = ();

    view! {
        root = gtk::Popover {
            set_position: gtk::PositionType::Top,
            set_autohide: true,
            set_has_arrow: false,
            set_size_request: (520, 360),
            connect_closed => HistoryMsg::Closed,

            #[wrap(Some)]
            set_child = &gtk::Box {
                set_orientation: gtk::Orientation::Vertical,
                set_spacing: 6,
                set_margin_all: 8,

                #[name(filter_entry)]
                gtk::SearchEntry {
                    set_placeholder_text: Some("Search history…  (try > for commands)"),
                    set_hexpand: true,
                    connect_search_changed[sender] => move |entry| {
                        sender.input(HistoryMsg::Search(entry.text().to_string()));
                    },
                },

                gtk::ScrolledWindow {
                    set_hexpand: true,
                    set_vexpand: true,

                    #[name(list_box)]
                    gtk::ListBox {
                        set_selection_mode: gtk::SelectionMode::Single,
                        add_css_class: "boxed-list",
                        connect_row_activated[sender] => move |_, row| {
                            sender.input(HistoryMsg::ActivateIndex(row.index()));
                        },
                    },
                },
            },

            add_controller = gtk::EventControllerKey {
                set_propagation_phase: gtk::PropagationPhase::Capture,
                connect_key_pressed[sender] => move |_, key, _, _| {
                    use gtk::gdk::Key;
                    let message = match key {
                        Key::Escape => Some(HistoryMsg::Close),
                        Key::Return | Key::KP_Enter => Some(HistoryMsg::AcceptSelected),
                        Key::Down => Some(HistoryMsg::Move(1)),
                        Key::Up => Some(HistoryMsg::Move(-1)),
                        _ => None,
                    };
                    if let Some(message) = message {
                        sender.input(message);
                        gtk::glib::Propagation::Stop
                    } else {
                        gtk::glib::Propagation::Proceed
                    }
                },
            },
        }
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let model = Self {
            keybindings: init.keybindings,
            workflows: init.workflows,
            history_path: None,
            query: String::new(),
            accepts: Vec::new(),
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
            HistoryMsg::Toggle {
                anchor,
                history_path,
            } => {
                if root.parent().is_some() {
                    root.popdown();
                    return;
                }
                self.history_path = history_path;
                self.query.clear();
                widgets.filter_entry.set_text("");
                self.rebuild_rows(widgets);
                root.set_parent(&anchor);
                root.popup();
                widgets.filter_entry.grab_focus();
            }
            HistoryMsg::Search(query) => {
                self.query = query;
                self.rebuild_rows(widgets);
            }
            HistoryMsg::ActivateIndex(index) => self.accept_index(index, &sender, root),
            HistoryMsg::Move(delta) => {
                let len = self.accepts.len() as i32;
                if len == 0 {
                    return;
                }
                let current = widgets
                    .list_box
                    .selected_row()
                    .map(|row| row.index())
                    .unwrap_or(0);
                let next = (current + delta).clamp(0, len - 1);
                if let Some(row) = widgets.list_box.row_at_index(next) {
                    widgets.list_box.select_row(Some(&row));
                }
            }
            HistoryMsg::AcceptSelected => {
                if let Some(row) = widgets.list_box.selected_row() {
                    self.accept_index(row.index(), &sender, root);
                }
            }
            HistoryMsg::Close => root.popdown(),
            HistoryMsg::Closed => root.unparent(),
        }
    }
}

impl HistoryModel {
    fn rebuild_rows(&mut self, widgets: &HistoryModelWidgets) {
        while let Some(row) = widgets.list_box.row_at_index(0) {
            widgets.list_box.remove(&row);
        }
        let query = Query::parse(&self.query, PaletteMode::History);
        let entries = palette::gather(
            &query,
            &self.keybindings.borrow(),
            self.history_path.as_deref(),
            &self.workflows.borrow(),
            100,
        );
        self.accepts.clear();
        for entry in entries {
            let row = adw::ActionRow::builder()
                .title(escape_markup(&entry.label))
                .subtitle(escape_markup(entry.sublabel.as_deref().unwrap_or("")))
                .activatable(true)
                .build();
            if let Some(right) = entry.right {
                let label = gtk::Label::new(Some(&right));
                label.add_css_class("dim-label");
                row.add_suffix(&label);
            }
            widgets.list_box.append(&row);
            self.accepts.push(entry.accept);
        }
        widgets
            .list_box
            .select_row(widgets.list_box.row_at_index(0).as_ref());
    }

    fn accept_index(&self, index: i32, sender: &ComponentSender<Self>, root: &gtk::Popover) {
        let Some(accept) = usize::try_from(index)
            .ok()
            .and_then(|index| self.accepts.get(index))
            .cloned()
        else {
            return;
        };
        root.popdown();
        let output = match accept {
            Accept::Action(action) => HistoryOutput::Action(action),
            Accept::TypeCommand(command) => HistoryOutput::TypeCommand(command),
            Accept::AskAi(query) => HistoryOutput::AskAi(query),
            Accept::RunWorkflow(path) => HistoryOutput::RunWorkflow(path),
        };
        let _ = sender.output(output);
    }
}

fn escape_markup(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
