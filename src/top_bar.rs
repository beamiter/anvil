//! Relm4 component for the main window toolbar.

use relm4::gtk;
use relm4::gtk::prelude::*;
use relm4::prelude::*;

#[derive(Debug)]
pub(crate) enum TopBarOutput {
    ToggleSidebar,
    ToggleTabPlacement,
    NewTab,
    Quit,
}

pub(crate) struct TopBarModel {
    tab_scroll: gtk::ScrolledWindow,
}

#[relm4::component(pub(crate))]
impl SimpleComponent for TopBarModel {
    type Init = gtk::ScrolledWindow;
    type Input = ();
    type Output = TopBarOutput;

    view! {
        root = gtk::Overlay {
            add_css_class: "top-bar",
            set_hexpand: true,
            set_height_request: 40,

            #[wrap(Some)]
            set_child = &gtk::Box {
                set_orientation: gtk::Orientation::Horizontal,
            },

            add_overlay = &gtk::Box {
                set_orientation: gtk::Orientation::Horizontal,
                set_spacing: 4,
                set_halign: gtk::Align::Start,
                set_valign: gtk::Align::Center,

                gtk::Button {
                    set_icon_name: "open-menu-symbolic",
                    set_focus_on_click: false,
                    set_can_focus: false,
                    set_tooltip_text: Some("Toggle sidebar (Ctrl+\\)"),
                    add_css_class: "flat",
                    connect_clicked[sender] => move |_| {
                        let _ = sender.output(TopBarOutput::ToggleSidebar);
                    },
                },

                gtk::Button {
                    set_icon_name: "view-list-symbolic",
                    set_focus_on_click: false,
                    set_can_focus: false,
                    set_tooltip_text: Some("Toggle tabs: sidebar / top bar"),
                    add_css_class: "flat",
                    connect_clicked[sender] => move |_| {
                        let _ = sender.output(TopBarOutput::ToggleTabPlacement);
                    },
                },
            },

            #[local_ref]
            add_overlay = tab_scroll -> gtk::ScrolledWindow {},

            add_overlay = &gtk::Box {
                set_orientation: gtk::Orientation::Horizontal,
                set_spacing: 4,
                set_halign: gtk::Align::End,
                set_valign: gtk::Align::Center,
                add_css_class: "top-bar-actions",

                gtk::Button {
                    set_icon_name: "list-add-symbolic",
                    set_focus_on_click: false,
                    set_can_focus: false,
                    set_tooltip_text: Some("New tab (Ctrl+Shift+T)"),
                    add_css_class: "flat",
                    connect_clicked[sender] => move |_| {
                        let _ = sender.output(TopBarOutput::NewTab);
                    },
                },

                gtk::Button {
                    set_icon_name: "window-close-symbolic",
                    set_focus_on_click: false,
                    set_can_focus: false,
                    set_tooltip_text: Some("Close window"),
                    add_css_class: "flat",
                    connect_clicked[sender] => move |_| {
                        let _ = sender.output(TopBarOutput::Quit);
                    },
                },
            },
        }
    }

    fn init(
        tab_scroll: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let model = Self { tab_scroll };
        let tab_scroll = &model.tab_scroll;
        let widgets = view_output!();
        ComponentParts { model, widgets }
    }
}
