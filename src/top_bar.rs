//! Relm4 component for the main window toolbar.

use relm4::gtk;
use relm4::gtk::prelude::*;
use relm4::prelude::*;

const OPEN_COMMAND_CENTER_LABEL: &str = "Open command center";
const TOGGLE_SIDEBAR_LABEL: &str = "Toggle sidebar";
const TOGGLE_TAB_PLACEMENT_LABEL: &str = "Move tabs between sidebar and top bar";
const ACTIVATE_AGENT_LABEL: &str = "Activate Shell Agent";
const CLOSE_AGENT_LABEL: &str = "Close Shell Agent";
const AGENT_UNAVAILABLE_LABEL: &str = "Shell Agent unavailable";
const NEW_TAB_LABEL: &str = "New tab";
const MINIMIZE_WINDOW_LABEL: &str = "Minimize window";
const MAXIMIZE_WINDOW_LABEL: &str = "Maximize window";
const RESTORE_WINDOW_LABEL: &str = "Restore window";
const CLOSE_WINDOW_LABEL: &str = "Close window";

fn maximize_presentation(maximized: bool) -> (&'static str, &'static str) {
    if maximized {
        ("window-restore-symbolic", RESTORE_WINDOW_LABEL)
    } else {
        ("window-maximize-symbolic", MAXIMIZE_WINDOW_LABEL)
    }
}

fn agent_presentation(available: bool, active: bool) -> (&'static str, &'static str) {
    if !available {
        (
            "Shell Agent is disabled in Settings or safe mode",
            AGENT_UNAVAILABLE_LABEL,
        )
    } else if active {
        ("Close Shell Agent (Ctrl+Alt+G)", CLOSE_AGENT_LABEL)
    } else {
        ("Activate Shell Agent (Ctrl+Alt+G)", ACTIVATE_AGENT_LABEL)
    }
}

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
                    update_property: &[
                        gtk::accessible::Property::Label(OPEN_COMMAND_CENTER_LABEL),
                    ],
                    add_css_class: "flat",
                    connect_clicked[sender] => move |_| {
                        let _ = sender.output(TopBarOutput::OpenPalette);
                    },
                },

                gtk::Button {
                    set_icon_name: "sidebar-show-symbolic",
                    set_focus_on_click: false,
                    set_tooltip_text: Some("Toggle sidebar (Ctrl+\\)"),
                    update_property: &[
                        gtk::accessible::Property::Label(TOGGLE_SIDEBAR_LABEL),
                    ],
                    add_css_class: "flat",
                    connect_clicked[sender] => move |_| {
                        let _ = sender.output(TopBarOutput::ToggleSidebar);
                    },
                },

                gtk::Button {
                    set_icon_name: "view-list-symbolic",
                    set_focus_on_click: false,
                    set_tooltip_text: Some("Toggle tabs: sidebar / top bar"),
                    update_property: &[
                        gtk::accessible::Property::Label(TOGGLE_TAB_PLACEMENT_LABEL),
                    ],
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
                    update_property: &[
                        gtk::accessible::Property::Label(ACTIVATE_AGENT_LABEL),
                    ],
                    add_css_class: "flat",
                    connect_clicked[sender] => move |_| {
                        let _ = sender.output(TopBarOutput::ToggleAgent);
                    },
                },

                gtk::Button {
                    set_icon_name: "list-add-symbolic",
                    set_focus_on_click: false,
                    set_tooltip_text: Some("New tab (Ctrl+Shift+T)"),
                    update_property: &[
                        gtk::accessible::Property::Label(NEW_TAB_LABEL),
                    ],
                    add_css_class: "flat",
                    connect_clicked[sender] => move |_| {
                        let _ = sender.output(TopBarOutput::NewTab);
                    },
                },

                gtk::Button {
                    set_icon_name: "window-minimize-symbolic",
                    set_focus_on_click: false,
                    set_tooltip_text: Some("Minimize window"),
                    update_property: &[
                        gtk::accessible::Property::Label(MINIMIZE_WINDOW_LABEL),
                    ],
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
                    update_property: &[
                        gtk::accessible::Property::Label(MAXIMIZE_WINDOW_LABEL),
                    ],
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
                    update_property: &[
                        gtk::accessible::Property::Label(CLOSE_WINDOW_LABEL),
                    ],
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
                let (icon_name, label) = maximize_presentation(maximized);
                widgets.maximize_button.set_icon_name(icon_name);
                widgets.maximize_button.set_tooltip_text(Some(label));
                widgets
                    .maximize_button
                    .update_property(&[gtk::accessible::Property::Label(label)]);
            }
            TopBarMsg::SetAgentState { available, active } => {
                widgets.agent_toggle.set_sensitive(available);
                widgets.agent_toggle.set_active(available && active);
                let (tooltip, label) = agent_presentation(available, active);
                widgets.agent_toggle.set_tooltip_text(Some(tooltip));
                widgets
                    .agent_toggle
                    .update_property(&[gtk::accessible::Property::Label(label)]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_top_bar_icon_button_has_a_distinct_accessible_label() {
        let labels = [
            OPEN_COMMAND_CENTER_LABEL,
            TOGGLE_SIDEBAR_LABEL,
            TOGGLE_TAB_PLACEMENT_LABEL,
            ACTIVATE_AGENT_LABEL,
            NEW_TAB_LABEL,
            MINIMIZE_WINDOW_LABEL,
            MAXIMIZE_WINDOW_LABEL,
            CLOSE_WINDOW_LABEL,
        ];
        assert!(labels.iter().all(|label| !label.is_empty()));
        for (index, label) in labels.iter().enumerate() {
            assert!(!labels[..index].contains(label), "duplicate label: {label}");
        }
    }

    #[test]
    fn dynamic_top_bar_controls_update_their_accessible_names() {
        assert_eq!(
            maximize_presentation(false),
            ("window-maximize-symbolic", MAXIMIZE_WINDOW_LABEL)
        );
        assert_eq!(
            maximize_presentation(true),
            ("window-restore-symbolic", RESTORE_WINDOW_LABEL)
        );
        assert_eq!(agent_presentation(true, false).1, ACTIVATE_AGENT_LABEL);
        assert_eq!(agent_presentation(true, true).1, CLOSE_AGENT_LABEL);
        assert_eq!(agent_presentation(false, true).1, AGENT_UNAVAILABLE_LABEL);
    }
}
