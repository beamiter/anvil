//! Small Relm4 components used by the sidebar pages.

use relm4::gtk;
use relm4::gtk::prelude::*;
use relm4::prelude::*;

#[derive(Debug)]
pub(crate) enum TabFilterMsg {
    Focus,
}

#[derive(Debug)]
pub(crate) enum TabFilterOutput {
    Changed(String),
}

pub(crate) struct TabFilterModel;

#[relm4::component(pub(crate))]
impl Component for TabFilterModel {
    type Init = ();
    type Input = TabFilterMsg;
    type Output = TabFilterOutput;
    type CommandOutput = ();

    view! {
        root = gtk::SearchEntry {
            set_placeholder_text: Some("Filter tabs…"),
            connect_search_changed[sender] => move |entry| {
                let _ = sender.output(TabFilterOutput::Changed(entry.text().to_string()));
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

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>, root: &Self::Root) {
        match msg {
            TabFilterMsg::Focus => {
                root.grab_focus();
            }
        }
    }
}

#[derive(Debug)]
pub(crate) enum FileHeaderMsg {
    SetRoot { display: String, tooltip: String },
}

#[derive(Debug)]
pub(crate) enum FileHeaderOutput {
    Up,
    CurrentDirectory,
}

pub(crate) struct FileHeaderModel {
    display: String,
    tooltip: String,
}

#[relm4::component(pub(crate))]
impl Component for FileHeaderModel {
    type Init = ();
    type Input = FileHeaderMsg;
    type Output = FileHeaderOutput;
    type CommandOutput = ();

    view! {
        root = gtk::Box {
            set_orientation: gtk::Orientation::Horizontal,
            set_spacing: 2,

            gtk::Button {
                set_icon_name: "go-up-symbolic",
                set_tooltip_text: Some("Parent directory"),
                connect_clicked[sender] => move |_| {
                    let _ = sender.output(FileHeaderOutput::Up);
                },
            },

            gtk::Button {
                set_icon_name: "go-home-symbolic",
                set_tooltip_text: Some("Go to current directory"),
                connect_clicked[sender] => move |_| {
                    let _ = sender.output(FileHeaderOutput::CurrentDirectory);
                },
            },

            #[name(root_label)]
            gtk::Label {
                set_label: "~",
                set_xalign: 0.0,
                set_hexpand: true,
                set_ellipsize: gtk::pango::EllipsizeMode::Start,
            },
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let model = Self {
            display: "~".to_string(),
            tooltip: String::new(),
        };
        let widgets = view_output!();
        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        msg: Self::Input,
        _sender: ComponentSender<Self>,
        _root: &Self::Root,
    ) {
        match msg {
            FileHeaderMsg::SetRoot { display, tooltip } => {
                self.display = display;
                self.tooltip = tooltip;
                widgets.root_label.set_label(&self.display);
                widgets.root_label.set_tooltip_text(Some(&self.tooltip));
            }
        }
    }
}
