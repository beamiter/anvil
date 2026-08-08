//! Relm4 component for the main window toolbar.

use relm4::gtk;
use relm4::gtk::prelude::*;
use relm4::prelude::*;

#[derive(Debug)]
pub(crate) enum TopBarOutput {
    OpenPalette,
    ToggleSidebar,
    ToggleTabPlacement,
    ToggleAgent,
    NewTab,
    MinimizeWindow,
    ToggleMaximizedWindow,
    Quit,
}

#[derive(Debug)]
pub(crate) enum TopBarMsg {
    SetMaximized(bool),
    SetAgentState { available: bool, active: bool },
}

pub(crate) struct TopBarModel {
    tab_scroll: gtk::ScrolledWindow,
}

#[relm4::component(pub(crate))]
impl Component for TopBarModel {
    type Init = gtk::ScrolledWindow;
    type Input = TopBarMsg;
    type Output = TopBarOutput;
    type CommandOutput = ();

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
                    set_icon_name: "system-search-symbolic",
                    set_focus_on_click: false,
                    set_tooltip_text: Some("Open command center (Ctrl+Shift+P)"),
                    add_css_class: "flat",
                    connect_clicked[sender] => move |_| {
                        let _ = sender.output(TopBarOutput::OpenPalette);
                    },
                },

                gtk::Button {
                    set_icon_name: "sidebar-show-symbolic",
                    set_focus_on_click: false,
                    set_tooltip_text: Some("Toggle sidebar (Ctrl+\\)"),
                    add_css_class: "flat",
                    connect_clicked[sender] => move |_| {
                        let _ = sender.output(TopBarOutput::ToggleSidebar);
                    },
                },

                gtk::Button {
                    set_icon_name: "view-list-symbolic",
                    set_focus_on_click: false,
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
                add_css_class: "window-controls",

                #[name(agent_toggle)]
                gtk::ToggleButton {
                    set_icon_name: "system-run-symbolic",
                    set_focus_on_click: false,
                    set_tooltip_text: Some("Activate Shell Agent (Ctrl+Alt+G)"),
                    add_css_class: "flat",
                    connect_clicked[sender] => move |_| {
                        let _ = sender.output(TopBarOutput::ToggleAgent);
                    },
                },

                gtk::Button {
                    set_icon_name: "list-add-symbolic",
                    set_focus_on_click: false,
                    set_tooltip_text: Some("New tab (Ctrl+Shift+T)"),
                    add_css_class: "flat",
                    connect_clicked[sender] => move |_| {
                        let _ = sender.output(TopBarOutput::NewTab);
                    },
                },

                gtk::Button {
                    set_icon_name: "window-minimize-symbolic",
                    set_focus_on_click: false,
                    set_tooltip_text: Some("Minimize window"),
                    add_css_class: "flat",
                    add_css_class: "window-control",
                    connect_clicked[sender] => move |_| {
                        let _ = sender.output(TopBarOutput::MinimizeWindow);
                    },
                },

                #[name(maximize_button)]
                gtk::Button {
                    set_icon_name: "window-maximize-symbolic",
                    set_focus_on_click: false,
                    set_tooltip_text: Some("Maximize window"),
                    add_css_class: "flat",
                    add_css_class: "window-control",
                    connect_clicked[sender] => move |_| {
                        let _ = sender.output(TopBarOutput::ToggleMaximizedWindow);
                    },
                },

                gtk::Button {
                    set_icon_name: "window-close-symbolic",
                    set_focus_on_click: false,
                    set_tooltip_text: Some("Close window"),
                    add_css_class: "flat",
                    add_css_class: "window-control",
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

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        msg: Self::Input,
        _sender: ComponentSender<Self>,
        _root: &Self::Root,
    ) {
        match msg {
            TopBarMsg::SetMaximized(maximized) => {
                let (icon_name, tooltip) = if maximized {
                    ("window-restore-symbolic", "Restore window")
                } else {
                    ("window-maximize-symbolic", "Maximize window")
                };
                widgets.maximize_button.set_icon_name(icon_name);
                widgets.maximize_button.set_tooltip_text(Some(tooltip));
            }
            TopBarMsg::SetAgentState { available, active } => {
                widgets.agent_toggle.set_sensitive(available);
                widgets.agent_toggle.set_active(available && active);
                widgets.agent_toggle.set_tooltip_text(Some(if available {
                    if active {
                        "Close Shell Agent (Ctrl+Alt+G)"
                    } else {
                        "Activate Shell Agent (Ctrl+Alt+G)"
                    }
                } else {
                    "Shell Agent is disabled in Settings or safe mode"
                }));
            }
        }
    }
}
