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
const TOGGLE_TASKS_LABEL: &str = "Toggle agent Tasks panel";
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

/// Tooltip and accessible name for the Tasks button. Attention is counted
/// by the shared dashboard rule; hidden tasks never count.
fn tasks_presentation(count: usize, summary: Option<&str>) -> (String, String) {
    let tooltip = match summary {
        Some(summary) if count > 0 => format!("{summary} · Toggle Tasks (Ctrl+Alt+Shift+T)"),
        _ => "Agent tasks · Toggle Tasks (Ctrl+Alt+Shift+T)".to_string(),
    };
    let label = if count > 0 {
        format!("{TOGGLE_TASKS_LABEL}, {count} need you")
    } else {
        TOGGLE_TASKS_LABEL.to_string()
    };
    (tooltip, label)
}

#[derive(Debug)]
pub(crate) enum TopBarOutput {
    OpenPalette,
    OpenVim9Ide,
    ToggleSidebar,
    ToggleTabPlacement,
    ToggleAgent,
    OpenAgentSessions,
    ToggleTasks,
    NewTab,
    MinimizeWindow,
    ToggleMaximizedWindow,
    Quit,
}

#[derive(Debug)]
pub(crate) enum TopBarMsg {
    SetMaximized(bool),
    SetAgentState {
        available: bool,
        active: bool,
    },
    /// Show the Tasks button only where tasks can be used, with the number
    /// of visible tasks needing a person and the shared explanation.
    TaskAttention {
        visible: bool,
        count: usize,
        summary: Option<String>,
    },
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

                gtk::Button {
                    set_icon_name: "text-editor-symbolic",
                    set_focus_on_click: false,
                    set_tooltip_text: Some("Open Vim9 IDE (Ctrl+Alt+I)"),
                    update_property: &[
                        gtk::accessible::Property::Label("Open Vim9 IDE"),
                        gtk::accessible::Property::KeyShortcuts("Control+Alt+I"),
                    ],
                    add_css_class: "flat",
                    connect_clicked[sender] => move |_| {
                        let _ = sender.output(TopBarOutput::OpenVim9Ide);
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

                gtk::Button {
                    set_icon_name: "view-grid-symbolic",
                    set_focus_on_click: false,
                    set_tooltip_text: Some("Agent Sessions: resume Codex / Claude (Ctrl+Alt+Shift+G)"),
                    update_property: &[gtk::accessible::Property::Label("Agent Sessions")],
                    add_css_class: "flat",
                    connect_clicked[sender] => move |_| {
                        let _ = sender.output(TopBarOutput::OpenAgentSessions);
                    },
                },

                #[name(tasks_button)]
                gtk::Button {
                    set_visible: false,
                    set_focus_on_click: false,
                    set_tooltip_text: Some("Agent tasks · Toggle Tasks (Ctrl+Alt+Shift+T)"),
                    update_property: &[
                        gtk::accessible::Property::Label(TOGGLE_TASKS_LABEL),
                        gtk::accessible::Property::KeyShortcuts("Control+Alt+Shift+T"),
                    ],
                    add_css_class: "flat",
                    connect_clicked[sender] => move |_| {
                        let _ = sender.output(TopBarOutput::ToggleTasks);
                    },

                    gtk::Box {
                        set_orientation: gtk::Orientation::Horizontal,
                        set_spacing: 4,

                        gtk::Image {
                            set_icon_name: Some("view-list-bullet-symbolic"),
                        },

                        #[name(tasks_badge)]
                        gtk::Label {
                            set_visible: false,
                            add_css_class: "caption-heading",
                            add_css_class: "accent",
                        },
                    },
                },

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
        let overlay = root.clone();
        let widgets = view_output!();
        // The tab row is an overlay: include its natural height so enlarged
        // text can grow the bar instead of being clipped to the old 40px floor.
        overlay.set_measure_overlay(tab_scroll, true);
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
            TopBarMsg::TaskAttention {
                visible,
                count,
                summary,
            } => {
                widgets.tasks_button.set_visible(visible);
                widgets.tasks_badge.set_visible(visible && count > 0);
                widgets.tasks_badge.set_label(&count.to_string());
                let (tooltip, label) = tasks_presentation(count, summary.as_deref());
                widgets.tasks_button.set_tooltip_text(Some(&tooltip));
                widgets
                    .tasks_button
                    .update_property(&[gtk::accessible::Property::Label(&label)]);
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
            TOGGLE_TASKS_LABEL,
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

    #[test]
    fn tasks_button_names_the_attention_count_and_shared_summary() {
        let (tooltip, label) = tasks_presentation(0, None);
        assert_eq!(label, TOGGLE_TASKS_LABEL);
        assert!(tooltip.contains("Ctrl+Alt+Shift+T"));
        let (tooltip, label) = tasks_presentation(2, Some("2 tasks need you: 1 approval, 1 reply"));
        assert_eq!(label, "Toggle agent Tasks panel, 2 need you");
        assert!(tooltip.starts_with("2 tasks need you: 1 approval, 1 reply"));
        // A stale summary without a count never claims attention.
        assert!(!tasks_presentation(0, Some("1 task needs you: 1 reply"))
            .0
            .contains("need you"));
    }
}
