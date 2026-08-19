//! Small Relm4 components used by the sidebar pages.

use relm4::gtk;
use relm4::gtk::prelude::*;
use relm4::prelude::*;

const PARENT_DIRECTORY_LABEL: &str = "Go to parent directory";
const CURRENT_DIRECTORY_LABEL: &str = "Go to current terminal directory";

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
    SetRoot {
        display: String,
        tooltip: String,
    },
    /// Rebuild the location selector's labels and selection without emitting
    /// `SelectLocation` (config edit or a rollback to Local).
    SetLocations {
        labels: Vec<String>,
        selected: usize,
    },
    /// The user moved the selector; internal, keeps programmatic rebuilds from
    /// echoing back as location switches.
    LocationActivated(usize),
    /// The magnifier toggle moved; opens/closes the inline filter entry.
    ToggleFilter,
    /// The filter entry's text changed (also fired when closing clears it).
    FilterEdited(String),
    /// Esc in the entry or a programmatic close (e.g. a root change).
    CloseFilter,
}

#[derive(Debug)]
pub(crate) enum FileHeaderOutput {
    Up,
    CurrentDirectory,
    /// Dropdown index: 0 is Local, i > 0 is `config.remote_hosts[i - 1]`.
    SelectLocation(usize),
    /// Current filter query; "" when the filter is closed or cleared.
    FilterChanged(String),
}

pub(crate) struct FileHeaderModel {
    display: String,
    tooltip: String,
    selected: usize,
    suppress_location_signal: bool,
    filter_open: bool,
}

#[relm4::component(pub(crate))]
impl Component for FileHeaderModel {
    /// Initial selector labels; index 0 must be "Local".
    type Init = Vec<String>;
    type Input = FileHeaderMsg;
    type Output = FileHeaderOutput;
    type CommandOutput = ();

    view! {
        root = gtk::Box {
            set_orientation: gtk::Orientation::Vertical,
            set_spacing: 2,

            gtk::Box {
                set_orientation: gtk::Orientation::Horizontal,
                set_spacing: 2,

                #[name(location_dropdown)]
                gtk::DropDown {
                    set_tooltip_text: Some("Choose which filesystem the tree browses"),
                    connect_selected_notify[sender] => move |dropdown| {
                        sender.input(FileHeaderMsg::LocationActivated(dropdown.selected() as usize));
                    },
                },

                gtk::Button {
                    set_icon_name: "go-up-symbolic",
                    set_tooltip_text: Some("Parent directory"),
                    update_property: &[
                        gtk::accessible::Property::Label(PARENT_DIRECTORY_LABEL),
                    ],
                    connect_clicked[sender] => move |_| {
                        let _ = sender.output(FileHeaderOutput::Up);
                    },
                },

                gtk::Button {
                    set_icon_name: "go-home-symbolic",
                    set_tooltip_text: Some("Go to current directory"),
                    update_property: &[
                        gtk::accessible::Property::Label(CURRENT_DIRECTORY_LABEL),
                    ],
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

                #[name(filter_button)]
                gtk::ToggleButton {
                    set_icon_name: "edit-find-symbolic",
                    set_tooltip_text: Some("Filter the loaded rows"),
                    connect_toggled[sender] => move |_| {
                        sender.input(FileHeaderMsg::ToggleFilter);
                    },
                },
            },

            #[name(filter_entry)]
            gtk::Entry {
                set_placeholder_text: Some("Filter loaded rows…"),
                set_visible: false,
                connect_changed[sender] => move |entry| {
                    sender.input(FileHeaderMsg::FilterEdited(entry.text().to_string()));
                },
            },
        }
    }

    fn init(
        labels: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let model = Self {
            display: "~".to_string(),
            tooltip: String::new(),
            selected: 0,
            suppress_location_signal: false,
            filter_open: false,
        };
        let widgets = view_output!();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        widgets
            .location_dropdown
            .set_model(Some(&gtk::StringList::new(&refs)));
        // Esc inside the entry closes the filter.
        let key_controller = gtk::EventControllerKey::new();
        {
            let sender = sender.clone();
            key_controller.connect_key_pressed(move |_, key, _, _| {
                if key == gtk::gdk::Key::Escape {
                    sender.input(FileHeaderMsg::CloseFilter);
                }
                gtk::glib::Propagation::Proceed
            });
        }
        widgets.filter_entry.add_controller(key_controller);
        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        msg: Self::Input,
        sender: ComponentSender<Self>,
        _root: &Self::Root,
    ) {
        match msg {
            FileHeaderMsg::SetRoot { display, tooltip } => {
                self.display = display;
                self.tooltip = tooltip;
                widgets.root_label.set_label(&self.display);
                widgets.root_label.set_tooltip_text(Some(&self.tooltip));
            }
            FileHeaderMsg::SetLocations { labels, selected } => {
                // set_model/set_selected fire `selected` notifications
                // synchronously; the flag keeps the rebuild from looking like
                // user input and switching the tree underneath itself.
                self.suppress_location_signal = true;
                let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
                widgets
                    .location_dropdown
                    .set_model(Some(&gtk::StringList::new(&refs)));
                widgets.location_dropdown.set_selected(selected as u32);
                self.selected = selected;
                self.suppress_location_signal = false;
            }
            FileHeaderMsg::LocationActivated(index) => {
                if !self.suppress_location_signal && index != self.selected {
                    self.selected = index;
                    let _ = sender.output(FileHeaderOutput::SelectLocation(index));
                }
            }
            FileHeaderMsg::ToggleFilter => {
                self.filter_open = widgets.filter_button.is_active();
                widgets.filter_entry.set_visible(self.filter_open);
                if self.filter_open {
                    widgets.filter_entry.grab_focus();
                } else {
                    // Clearing the text fires FilterEdited(""), which resets
                    // the tree filter through the output channel.
                    widgets.filter_entry.set_text("");
                }
            }
            FileHeaderMsg::FilterEdited(text) => {
                let _ = sender.output(FileHeaderOutput::FilterChanged(text));
            }
            FileHeaderMsg::CloseFilter => {
                if self.filter_open {
                    widgets.filter_button.set_active(false);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_header_icon_buttons_have_distinct_accessible_labels() {
        assert!(!PARENT_DIRECTORY_LABEL.is_empty());
        assert!(!CURRENT_DIRECTORY_LABEL.is_empty());
        assert_ne!(PARENT_DIRECTORY_LABEL, CURRENT_DIRECTORY_LABEL);
    }
}
