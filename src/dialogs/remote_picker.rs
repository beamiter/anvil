//! Relm4 component for selecting a configured remote host.

use relm4::adw;
use relm4::adw::prelude::*;
use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender, FactoryVecDeque};
use relm4::gtk;
use relm4::prelude::*;

use crate::config::RemoteHost;

#[derive(Debug)]
struct RemoteRow {
    source_index: usize,
    name: String,
    target: String,
}

#[derive(Debug)]
enum RemoteRowOutput {
    Activate(usize),
}

#[relm4::factory]
impl FactoryComponent for RemoteRow {
    type Init = (usize, RemoteHost);
    type Input = ();
    type Output = RemoteRowOutput;
    type CommandOutput = ();
    type ParentWidget = gtk::ListBox;

    view! {
        root = adw::ActionRow {
            set_title: &self.name,
            set_subtitle: &self.target,
            set_activatable: true,
            connect_activated[sender, source_index = self.source_index] => move |_| {
                let _ = sender.output(RemoteRowOutput::Activate(source_index));
            },
        }
    }

    fn init_model(
        (source_index, host): Self::Init,
        _index: &DynamicIndex,
        _sender: FactorySender<Self>,
    ) -> Self {
        let target = match host.user {
            Some(user) => format!("{user}@{}", host.host),
            None => host.host,
        };
        Self {
            source_index,
            name: jterm_core::review_input::safe_inline_display(&host.name, 256),
            target: jterm_core::review_input::safe_inline_display(&target, 512),
        }
    }
}

#[derive(Debug)]
pub(crate) enum RemotePickerMsg {
    Toggle(Vec<(usize, RemoteHost)>),
    Search(String),
    Activate(usize),
    Move(i32),
    Accept,
    Close,
}

#[derive(Debug)]
pub(crate) enum RemotePickerOutput {
    Connect(usize),
}

pub(crate) struct RemotePickerModel {
    parent: adw::ApplicationWindow,
    hosts: Vec<(usize, RemoteHost)>,
    query: String,
    rows: FactoryVecDeque<RemoteRow>,
}

#[relm4::component(pub(crate))]
impl Component for RemotePickerModel {
    type Init = adw::ApplicationWindow;
    type Input = RemotePickerMsg;
    type Output = RemotePickerOutput;
    type CommandOutput = ();

    view! {
        root = adw::Dialog {
            set_title: "Connect to Remote Host",
            set_content_width: 480,
            set_content_height: 480,

            #[wrap(Some)]
            set_child = &adw::ToolbarView {
                add_top_bar = &adw::HeaderBar {},

                #[wrap(Some)]
                set_content = &gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,

                    #[name(filter_entry)]
                    gtk::SearchEntry {
                        set_placeholder_text: Some("Search hosts…"),
                        set_hexpand: true,
                        set_margin_all: 12,
                        connect_search_changed[sender] => move |entry| {
                            sender.input(RemotePickerMsg::Search(entry.text().to_string()));
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
                connect_key_pressed[sender] => move |_, key, _, _| {
                    use gtk::gdk::Key;
                    let message = match key {
                        Key::Escape => Some(RemotePickerMsg::Close),
                        Key::Return | Key::KP_Enter => Some(RemotePickerMsg::Accept),
                        Key::Down => Some(RemotePickerMsg::Move(1)),
                        Key::Up => Some(RemotePickerMsg::Move(-1)),
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
        parent: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let rows =
            FactoryVecDeque::builder()
                .launch_default()
                .forward(sender.input_sender(), |output| match output {
                    RemoteRowOutput::Activate(index) => RemotePickerMsg::Activate(index),
                });
        let model = Self {
            parent,
            hosts: Vec::new(),
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
            RemotePickerMsg::Toggle(hosts) => {
                if root.parent().is_some() {
                    root.force_close();
                    return;
                }
                if hosts.is_empty() {
                    log::warn!("[remote] no remote_hosts configured; nothing to pick");
                    return;
                }
                self.hosts = hosts;
                self.query.clear();
                widgets.filter_entry.set_text("");
                self.rebuild_rows();
                self.select_first(widgets);
                root.present(Some(&self.parent));
                widgets.filter_entry.grab_focus();
            }
            RemotePickerMsg::Search(query) => {
                self.query = query;
                self.rebuild_rows();
                self.select_first(widgets);
            }
            RemotePickerMsg::Activate(index) => {
                root.force_close();
                let _ = sender.output(RemotePickerOutput::Connect(index));
            }
            RemotePickerMsg::Move(delta) => {
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
            RemotePickerMsg::Accept => {
                let Some(row) = widgets.list_box.selected_row() else {
                    return;
                };
                let source_index = self.rows.guard()[row.index() as usize].source_index;
                root.force_close();
                let _ = sender.output(RemotePickerOutput::Connect(source_index));
            }
            RemotePickerMsg::Close => root.force_close(),
        }
    }
}

impl RemotePickerModel {
    fn rebuild_rows(&mut self) {
        let query = self.query.trim().to_lowercase();
        let mut rows = self.rows.guard();
        rows.clear();
        for (source_index, host) in &self.hosts {
            let target = match &host.user {
                Some(user) => format!("{user}@{}", host.host),
                None => host.host.clone(),
            };
            let name = jterm_core::review_input::safe_inline_display(&host.name, 256);
            let target = jterm_core::review_input::safe_inline_display(&target, 512);
            let haystack = format!("{name} {target}").to_lowercase();
            if query.is_empty() || haystack.contains(&query) {
                rows.push_back((*source_index, host.clone()));
            }
        }
    }

    fn select_first(&self, widgets: &RemotePickerModelWidgets) {
        widgets
            .list_box
            .select_row(widgets.list_box.row_at_index(0).as_ref());
    }
}
