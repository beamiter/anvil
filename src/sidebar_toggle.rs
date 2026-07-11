//! Relm4 component for switching the sidebar between tabs and files.

use relm4::gtk;
use relm4::gtk::prelude::*;
use relm4::prelude::*;

use crate::config::SidebarView;

#[derive(Debug)]
pub(crate) enum SidebarToggleMsg {
    SetView(SidebarView),
    SetTabsEnabled(bool),
}

#[derive(Debug)]
pub(crate) enum SidebarToggleOutput {
    View(SidebarView),
}

pub(crate) struct SidebarToggleModel {
    view: SidebarView,
    tabs_enabled: bool,
}

#[relm4::component(pub(crate))]
impl Component for SidebarToggleModel {
    type Init = (SidebarView, bool);
    type Input = SidebarToggleMsg;
    type Output = SidebarToggleOutput;
    type CommandOutput = ();

    view! {
        root = gtk::Box {
            set_orientation: gtk::Orientation::Horizontal,
            add_css_class: "sidebar-toggle-row",

            #[name(tabs_button)]
            gtk::ToggleButton {
                set_label: "Tabs",
                set_hexpand: true,
                set_active: model.view == SidebarView::Tabs,
                set_sensitive: model.tabs_enabled,
                add_css_class: "sidebar-toggle",
                connect_clicked[sender] => move |button| {
                    if button.is_active() {
                        let _ = sender.output(SidebarToggleOutput::View(SidebarView::Tabs));
                    }
                },
            },

            #[name(files_button)]
            gtk::ToggleButton {
                set_label: "Files",
                set_hexpand: true,
                set_group: Some(&tabs_button),
                set_active: model.view == SidebarView::Files,
                add_css_class: "sidebar-toggle",
                connect_clicked[sender] => move |button| {
                    if button.is_active() {
                        let _ = sender.output(SidebarToggleOutput::View(SidebarView::Files));
                    }
                },
            },
        }
    }

    fn init(
        (view, tabs_enabled): Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let model = Self { view, tabs_enabled };
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
            SidebarToggleMsg::SetView(view) => {
                self.view = view;
                widgets.tabs_button.set_active(view == SidebarView::Tabs);
                widgets.files_button.set_active(view == SidebarView::Files);
            }
            SidebarToggleMsg::SetTabsEnabled(enabled) => {
                self.tabs_enabled = enabled;
                widgets.tabs_button.set_sensitive(enabled);
            }
        }
    }
}
