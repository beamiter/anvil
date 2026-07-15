//! Relm4 command-palette component backed by a factory list.

use relm4::adw;
use relm4::adw::prelude::*;
use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender, FactoryVecDeque};
use relm4::gtk;
use relm4::prelude::*;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use crate::keybindings::{Action, KeybindingMap};
use crate::palette::{self as palette_data, Accept, Entry, PaletteMode, Query};
use crate::workflows::Workflow;

#[derive(Debug)]
struct PaletteRow {
    label: String,
    sublabel: Option<String>,
    right: Option<String>,
    accept: Accept,
}

#[derive(Debug)]
enum PaletteRowOutput {
    Activate(Accept),
}

#[relm4::factory]
impl FactoryComponent for PaletteRow {
    type Init = Entry;
    type Input = ();
    type Output = PaletteRowOutput;
    type CommandOutput = ();
    type ParentWidget = gtk::ListBox;

    view! {
        root = adw::ActionRow {
            set_title: &escape_markup(&self.label),
            set_subtitle: &escape_markup(self.sublabel.as_deref().unwrap_or("")),
            set_activatable: true,
            connect_activated[sender, accept = self.accept.clone()] => move |_| {
                let _ = sender.output(PaletteRowOutput::Activate(accept.clone()));
            },

            add_suffix = &gtk::Label {
                set_label: self.right.as_deref().unwrap_or(""),
                set_visible: self.right.is_some(),
                add_css_class: "dim-label",
            },
        }
    }

    fn init_model(entry: Self::Init, _index: &DynamicIndex, _sender: FactorySender<Self>) -> Self {
        Self {
            label: entry.label,
            sublabel: entry.sublabel,
            right: entry.right,
            accept: entry.accept,
        }
    }
}

pub(crate) struct PaletteInit {
    pub(crate) parent: adw::ApplicationWindow,
    pub(crate) keybindings: Rc<RefCell<KeybindingMap>>,
    pub(crate) workflows: Rc<RefCell<Vec<Workflow>>>,
}

#[derive(Debug)]
pub(crate) enum PaletteMsg {
    Toggle {
        mode: PaletteMode,
        history_path: Option<PathBuf>,
    },
    Search(String),
    Activate(Accept),
    Move(i32),
    AcceptSelected,
    Close,
}

#[derive(Debug)]
pub(crate) enum PaletteOutput {
    Action(Action),
    TypeCommand(String),
    AskAi(String),
    RunWorkflow(PathBuf),
}

pub(crate) struct PaletteModel {
    parent: adw::ApplicationWindow,
    keybindings: Rc<RefCell<KeybindingMap>>,
    workflows: Rc<RefCell<Vec<Workflow>>>,
    mode: PaletteMode,
    history_path: Option<PathBuf>,
    query: String,
    rows: FactoryVecDeque<PaletteRow>,
}

#[relm4::component(pub(crate))]
impl Component for PaletteModel {
    type Init = PaletteInit;
    type Input = PaletteMsg;
    type Output = PaletteOutput;
    type CommandOutput = ();

    view! {
        root = adw::Dialog {
            set_title: "Palette",
            set_content_width: 560,
            set_content_height: 520,

            #[wrap(Some)]
            set_child = &adw::ToolbarView {
                add_top_bar = &adw::HeaderBar {},

                #[wrap(Some)]
                set_content = &gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,

                    #[name(filter_entry)]
                    gtk::SearchEntry {
                        set_hexpand: true,
                        set_margin_all: 12,
                        connect_search_changed[sender] => move |entry| {
                            sender.input(PaletteMsg::Search(entry.text().to_string()));
                        },
                    },

                    gtk::ScrolledWindow {
                        set_hexpand: true,
                        set_vexpand: true,

                        #[local_ref]
                        list_box -> gtk::ListBox {
                            set_selection_mode: gtk::SelectionMode::Single,
                            add_css_class: "boxed-list",
                            set_margin_start: 12,
                            set_margin_end: 12,
                            set_margin_bottom: 12,
                        },
                    },
                },
            },

            add_controller = gtk::EventControllerKey {
                set_propagation_phase: gtk::PropagationPhase::Capture,
                connect_key_pressed[sender] => move |_, key, _, state| {
                    use gtk::gdk::{Key, ModifierType};
                    let close_shortcut = matches!(key, Key::P | Key::p)
                        && state.contains(
                            ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK
                        );
                    let message = match key {
                        Key::Escape => Some(PaletteMsg::Close),
                        Key::Return | Key::KP_Enter => Some(PaletteMsg::AcceptSelected),
                        Key::Down => Some(PaletteMsg::Move(1)),
                        Key::Up => Some(PaletteMsg::Move(-1)),
                        _ if close_shortcut => Some(PaletteMsg::Close),
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
        let rows =
            FactoryVecDeque::builder()
                .launch_default()
                .forward(sender.input_sender(), |output| match output {
                    PaletteRowOutput::Activate(accept) => PaletteMsg::Activate(accept),
                });
        let model = Self {
            parent: init.parent,
            keybindings: init.keybindings,
            workflows: init.workflows,
            mode: PaletteMode::All,
            history_path: None,
            query: String::new(),
            rows,
        };
        let list_box = model.rows.widget();
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
            PaletteMsg::Toggle { mode, history_path } => {
                if root.parent().is_some() {
                    root.force_close();
                    return;
                }
                self.mode = mode;
                self.history_path = history_path;
                self.query.clear();
                root.set_title(title(mode));
                widgets
                    .filter_entry
                    .set_placeholder_text(Some(placeholder(mode)));
                widgets.filter_entry.set_text("");
                self.rebuild_rows();
                self.select_first(widgets);
                root.present(Some(&self.parent));
                widgets.filter_entry.grab_focus();
            }
            PaletteMsg::Search(query) => {
                self.query = query;
                self.rebuild_rows();
                self.select_first(widgets);
            }
            PaletteMsg::Activate(accept) => self.accept(accept, &sender, root),
            PaletteMsg::Move(delta) => {
                let len = self.rows.len() as i32;
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
            PaletteMsg::AcceptSelected => {
                let Some(row) = widgets.list_box.selected_row() else {
                    return;
                };
                let accept = self.rows.guard()[row.index() as usize].accept.clone();
                self.accept(accept, &sender, root);
            }
            PaletteMsg::Close => root.force_close(),
        }
    }
}

impl PaletteModel {
    fn rebuild_rows(&mut self) {
        let query = Query::parse(&self.query, self.mode);
        let entries = palette_data::gather(
            &query,
            &self.keybindings.borrow(),
            self.history_path.as_deref(),
            &self.workflows.borrow(),
            200,
        );
        let mut rows = self.rows.guard();
        rows.clear();
        for entry in entries {
            rows.push_back(entry);
        }
    }

    fn select_first(&self, widgets: &PaletteModelWidgets) {
        widgets
            .list_box
            .select_row(widgets.list_box.row_at_index(0).as_ref());
    }

    fn accept(&self, accept: Accept, sender: &ComponentSender<Self>, root: &adw::Dialog) {
        root.force_close();
        let output = match accept {
            Accept::Action(action) => PaletteOutput::Action(action),
            Accept::TypeCommand(command) => PaletteOutput::TypeCommand(command),
            Accept::AskAi(query) => PaletteOutput::AskAi(query),
            Accept::RunWorkflow(path) => PaletteOutput::RunWorkflow(path),
        };
        let _ = sender.output(output);
    }
}

fn title(mode: PaletteMode) -> &'static str {
    match mode {
        PaletteMode::All => "Palette",
        PaletteMode::Commands => "Command Palette",
        PaletteMode::History => "History",
        PaletteMode::Ai => "Ask AI",
        PaletteMode::Workflows => "Workflows",
    }
}

fn placeholder(mode: PaletteMode) -> &'static str {
    match mode {
        PaletteMode::All => "Search everything…  (> commands, @ history, : workflows, ? AI)",
        PaletteMode::Commands => "Search commands…  (@ history, : workflows, ? AI)",
        PaletteMode::History => "Search history…  (> commands, : workflows, ? AI)",
        PaletteMode::Ai => "Describe what you want…",
        PaletteMode::Workflows => "Search workflows…  (> commands, @ history)",
    }
}

fn escape_markup(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
