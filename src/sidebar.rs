//! Small Relm4 components used by the sidebar pages.

use std::cell::Cell;
use std::rc::Rc;

use relm4::gtk;
use relm4::gtk::prelude::*;
use relm4::prelude::*;

const PARENT_DIRECTORY_LABEL: &str = "Go to parent directory";
const HOME_DIRECTORY_LABEL: &str = "Go to filesystem home directory";
const BACK_DIRECTORY_LABEL: &str = "Go back in file-tree history";
const FORWARD_DIRECTORY_LABEL: &str = "Go forward in file-tree history";
const CURRENT_DIRECTORY_LABEL: &str = "Go to current terminal directory";
const OPEN_TERMINAL_LABEL: &str = "Open terminal for current file-tree location";
const SHOW_HIDDEN_LABEL: &str = "Show hidden files";
const MAX_BREADCRUMB_COMPONENTS: usize = 32;
const LOCAL_TERMINAL_TOOLTIP: &str = "Open a local terminal in this tree directory";
const REMOTE_TERMINAL_TOOLTIP: &str = "Connect to this remote profile; the remote shell chooses its start directory, which may differ from this tree path";

fn terminal_button_tooltip(selected_location: usize) -> &'static str {
    if selected_location == 0 {
        LOCAL_TERMINAL_TOOLTIP
    } else {
        REMOTE_TERMINAL_TOOLTIP
    }
}

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
        path: std::path::PathBuf,
    },
    SetNavigationAvailable {
        back: bool,
        forward: bool,
    },
    OpenPathEntry,
    ClosePathEntry,
    /// Rebuild the location selector's labels and selection without emitting
    /// `SelectLocation` (config edit or a rollback to Local).
    SetLocations {
        labels: Vec<String>,
        /// Full endpoint descriptions corresponding one-for-one to `labels`.
        details: Vec<String>,
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
    /// The eye toggle changed; hidden rows are filtered client-side.
    ToggleHidden,
}

#[derive(Debug)]
pub(crate) enum FileHeaderOutput {
    Back,
    Forward,
    Up,
    Home,
    CurrentDirectory,
    OpenTerminal,
    /// Dropdown index: 0 is Local, i > 0 is `config.remote_hosts[i - 1]`.
    SelectLocation(usize),
    /// Current filter query; "" when the filter is closed or cleared.
    FilterChanged(String),
    /// Whether dot-prefixed entries should be visible.
    ShowHiddenChanged(bool),
    NavigatePath(std::path::PathBuf),
    PathEntered(String),
}

pub(crate) struct FileHeaderModel {
    display: String,
    tooltip: String,
    location_details: Vec<String>,
    selected: usize,
    /// Raised only while a programmatic rebuild is driving the dropdown.
    /// The `notify::selected` handler reads it *inside* the emission, so the
    /// echo never becomes a queued message: `set_model`/`set_selected` emit
    /// synchronously, but `sender.input` would deliver after the rebuild has
    /// already lowered the flag, and the stale index would then read as a
    /// user-driven switch back to Local.
    suppress_location_signal: Rc<Cell<bool>>,
    filter_open: bool,
    show_hidden: bool,
}

#[relm4::component(pub(crate))]
impl Component for FileHeaderModel {
    /// Initial selector labels and their full endpoint details. Index 0 must
    /// describe the local filesystem in both vectors.
    type Init = (Vec<String>, Vec<String>);
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
                    set_widget_name: "file-tree-location",
                    set_hexpand: false,
                    set_tooltip_text: Some("Choose which filesystem the tree browses"),
                    connect_selected_notify[sender, suppress_location_signal] => move |dropdown| {
                        if suppress_location_signal.get() {
                            return;
                        }
                        sender.input(FileHeaderMsg::LocationActivated(dropdown.selected() as usize));
                    },
                },

                #[name(back_button)]
                gtk::Button {
                    set_icon_name: "go-previous-symbolic",
                    set_sensitive: false,
                    set_tooltip_text: Some("Back"),
                    update_property: &[
                        gtk::accessible::Property::Label(BACK_DIRECTORY_LABEL),
                    ],
                    connect_clicked[sender] => move |_| {
                        let _ = sender.output(FileHeaderOutput::Back);
                    },
                },

                #[name(forward_button)]
                gtk::Button {
                    set_icon_name: "go-next-symbolic",
                    set_sensitive: false,
                    set_tooltip_text: Some("Forward"),
                    update_property: &[
                        gtk::accessible::Property::Label(FORWARD_DIRECTORY_LABEL),
                    ],
                    connect_clicked[sender] => move |_| {
                        let _ = sender.output(FileHeaderOutput::Forward);
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
                    set_tooltip_text: Some("Filesystem home directory"),
                    update_property: &[
                        gtk::accessible::Property::Label(HOME_DIRECTORY_LABEL),
                    ],
                    connect_clicked[sender] => move |_| {
                        let _ = sender.output(FileHeaderOutput::Home);
                    },
                },

                gtk::Button {
                    set_icon_name: "go-jump-symbolic",
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

                #[name(terminal_button)]
                gtk::Button {
                    set_widget_name: "file-tree-open-terminal",
                    set_icon_name: "utilities-terminal-symbolic",
                    set_focusable: true,
                    set_tooltip_text: Some(LOCAL_TERMINAL_TOOLTIP),
                    update_property: &[
                        gtk::accessible::Property::Label(OPEN_TERMINAL_LABEL),
                    ],
                    connect_clicked[sender] => move |_| {
                        let _ = sender.output(FileHeaderOutput::OpenTerminal);
                    },
                },

                #[name(filter_button)]
                gtk::ToggleButton {
                    set_icon_name: "edit-find-symbolic",
                    set_tooltip_text: Some("Filter the loaded rows"),
                    connect_toggled[sender] => move |_| {
                        sender.input(FileHeaderMsg::ToggleFilter);
                    },
                },

                #[name(hidden_button)]
                gtk::ToggleButton {
                    set_widget_name: "file-tree-show-hidden",
                    set_icon_name: "view-reveal-symbolic",
                    set_focusable: true,
                    set_tooltip_text: Some("Show hidden files"),
                    update_property: &[
                        gtk::accessible::Property::Label(SHOW_HIDDEN_LABEL),
                    ],
                    connect_toggled[sender] => move |_| {
                        sender.input(FileHeaderMsg::ToggleHidden);
                    },
                },
            },

            #[name(breadcrumb_box)]
            gtk::Box {
                set_orientation: gtk::Orientation::Horizontal,
                set_spacing: 2,
                set_hexpand: true,
            },

            #[name(path_entry)]
            gtk::Entry {
                set_widget_name: "file-tree-path-entry",
                set_placeholder_text: Some("Enter an absolute path…"),
                set_max_length: 4096,
                set_visible: false,
                connect_activate[sender] => move |entry| {
                    let _ = sender.output(FileHeaderOutput::PathEntered(entry.text().to_string()));
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
        (labels, mut details): Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        if details.len() != labels.len() {
            // A mismatched rebuild must not attach the wrong authority to a
            // visible label. Falling back to the labels is safe and honest.
            details = labels.clone();
        }
        let suppress_location_signal = Rc::new(Cell::new(false));
        let model = Self {
            display: "~".to_string(),
            tooltip: String::new(),
            location_details: details,
            selected: 0,
            suppress_location_signal: suppress_location_signal.clone(),
            filter_open: false,
            show_hidden: false,
        };
        let widgets = view_output!();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        model.suppress_location_signal.set(true);
        widgets
            .location_dropdown
            .set_model(Some(&gtk::StringList::new(&refs)));
        model.suppress_location_signal.set(false);
        widgets.location_dropdown.set_tooltip_text(Some(
            model
                .location_details
                .first()
                .map(String::as_str)
                .unwrap_or("Choose which filesystem the tree browses"),
        ));
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
        let path_key_controller = gtk::EventControllerKey::new();
        {
            let sender = sender.clone();
            path_key_controller.connect_key_pressed(move |_, key, _, _| {
                if key == gtk::gdk::Key::Escape {
                    sender.input(FileHeaderMsg::ClosePathEntry);
                    return gtk::glib::Propagation::Stop;
                }
                gtk::glib::Propagation::Proceed
            });
        }
        widgets.path_entry.add_controller(path_key_controller);
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
            FileHeaderMsg::SetRoot {
                display,
                tooltip,
                path,
            } => {
                self.display = display;
                self.tooltip = tooltip;
                widgets.root_label.set_label(&self.display);
                widgets.root_label.set_tooltip_text(Some(&self.tooltip));
                while let Some(child) = widgets.breadcrumb_box.first_child() {
                    widgets.breadcrumb_box.remove(&child);
                }
                let root_button = gtk::Button::with_label("/");
                root_button.add_css_class("flat");
                {
                    let sender = sender.clone();
                    root_button.connect_clicked(move |_| {
                        let _ = sender.output(FileHeaderOutput::NavigatePath(
                            std::path::PathBuf::from("/"),
                        ));
                    });
                }
                widgets.breadcrumb_box.append(&root_button);
                let components: Vec<_> = path
                    .components()
                    .filter_map(|component| match component {
                        std::path::Component::Normal(name) => Some(name.to_os_string()),
                        _ => None,
                    })
                    .collect();
                let hidden = components.len().saturating_sub(MAX_BREADCRUMB_COMPONENTS);
                if hidden > 0 {
                    let separator = gtk::Label::new(Some("›"));
                    widgets.breadcrumb_box.append(&separator);
                    widgets.breadcrumb_box.append(&gtk::Label::new(Some("…")));
                }
                let mut prefix = std::path::PathBuf::from("/");
                for (index, name) in components.into_iter().enumerate() {
                    prefix.push(&name);
                    if index < hidden {
                        continue;
                    }
                    let separator = gtk::Label::new(Some("›"));
                    widgets.breadcrumb_box.append(&separator);
                    let label =
                        crate::review_input::safe_inline_display(&name.to_string_lossy(), 128);
                    let button = gtk::Button::with_label(&label);
                    button.add_css_class("flat");
                    button.set_tooltip_text(Some(&crate::file_tree::display_full_path(&prefix)));
                    {
                        let sender = sender.clone();
                        let path = prefix.clone();
                        button.connect_clicked(move |_| {
                            let _ = sender.output(FileHeaderOutput::NavigatePath(path.clone()));
                        });
                    }
                    widgets.breadcrumb_box.append(&button);
                }
                if let Some(path) = path.to_str() {
                    widgets.path_entry.set_text(path);
                    widgets.path_entry.set_tooltip_text(None);
                } else {
                    // Never round-trip a lossy local name into an actionable
                    // path. The user may still type a different UTF-8 path.
                    widgets.path_entry.set_text("");
                    widgets.path_entry.set_tooltip_text(Some(
                        "The current path is not valid UTF-8 and cannot be copied into this entry",
                    ));
                }
            }
            FileHeaderMsg::SetNavigationAvailable { back, forward } => {
                widgets.back_button.set_sensitive(back);
                widgets.forward_button.set_sensitive(forward);
            }
            FileHeaderMsg::OpenPathEntry => {
                widgets.path_entry.set_visible(true);
                widgets.path_entry.grab_focus();
                widgets.path_entry.select_region(0, -1);
            }
            FileHeaderMsg::ClosePathEntry => widgets.path_entry.set_visible(false),
            FileHeaderMsg::SetLocations {
                labels,
                mut details,
                selected,
            } => {
                // set_model/set_selected fire `selected` notifications
                // synchronously; the flag keeps the rebuild from looking like
                // user input and switching the tree underneath itself.
                self.suppress_location_signal.set(true);
                if details.len() != labels.len() {
                    details = labels.clone();
                }
                self.location_details = details;
                let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
                widgets
                    .location_dropdown
                    .set_model(Some(&gtk::StringList::new(&refs)));
                widgets.location_dropdown.set_selected(selected as u32);
                self.selected = selected;
                widgets.location_dropdown.set_tooltip_text(Some(
                    self.location_details
                        .get(selected)
                        .map(String::as_str)
                        .unwrap_or("Choose which filesystem the tree browses"),
                ));
                widgets
                    .terminal_button
                    .set_tooltip_text(Some(terminal_button_tooltip(selected)));
                self.suppress_location_signal.set(false);
            }
            FileHeaderMsg::LocationActivated(index) => {
                if !self.suppress_location_signal.get() && index != self.selected {
                    self.selected = index;
                    widgets.location_dropdown.set_tooltip_text(Some(
                        self.location_details
                            .get(index)
                            .map(String::as_str)
                            .unwrap_or("Choose which filesystem the tree browses"),
                    ));
                    widgets
                        .terminal_button
                        .set_tooltip_text(Some(terminal_button_tooltip(index)));
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
            FileHeaderMsg::ToggleHidden => {
                self.show_hidden = widgets.hidden_button.is_active();
                let _ = sender.output(FileHeaderOutput::ShowHiddenChanged(self.show_hidden));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descendant_named(root: &gtk::Widget, name: &str) -> Option<gtk::Widget> {
        let mut child = root.first_child();
        while let Some(widget) = child {
            if widget.widget_name() == name {
                return Some(widget);
            }
            if let Some(found) = descendant_named(&widget, name) {
                return Some(found);
            }
            child = widget.next_sibling();
        }
        None
    }

    #[test]
    fn file_header_icon_buttons_have_distinct_accessible_labels() {
        assert!(!PARENT_DIRECTORY_LABEL.is_empty());
        assert!(!HOME_DIRECTORY_LABEL.is_empty());
        assert!(!BACK_DIRECTORY_LABEL.is_empty());
        assert!(!FORWARD_DIRECTORY_LABEL.is_empty());
        assert!(!CURRENT_DIRECTORY_LABEL.is_empty());
        assert!(!OPEN_TERMINAL_LABEL.is_empty());
        assert!(!SHOW_HIDDEN_LABEL.is_empty());
        assert_ne!(PARENT_DIRECTORY_LABEL, CURRENT_DIRECTORY_LABEL);
        assert_ne!(BACK_DIRECTORY_LABEL, FORWARD_DIRECTORY_LABEL);
        assert_ne!(PARENT_DIRECTORY_LABEL, HOME_DIRECTORY_LABEL);
        assert_ne!(HOME_DIRECTORY_LABEL, CURRENT_DIRECTORY_LABEL);
        assert_ne!(HOME_DIRECTORY_LABEL, OPEN_TERMINAL_LABEL);
        assert_ne!(PARENT_DIRECTORY_LABEL, OPEN_TERMINAL_LABEL);
        assert_ne!(CURRENT_DIRECTORY_LABEL, OPEN_TERMINAL_LABEL);
        assert_ne!(OPEN_TERMINAL_LABEL, SHOW_HIDDEN_LABEL);
    }

    #[test]
    fn terminal_tooltip_is_honest_about_local_and_remote_cwd_semantics() {
        assert!(terminal_button_tooltip(0).contains("this tree directory"));
        assert!(terminal_button_tooltip(1).contains("may differ from this tree path"));
        assert_eq!(terminal_button_tooltip(128), REMOTE_TERMINAL_TOOLTIP);
    }

    /// A programmatic selector rebuild must not read back as a user switch.
    /// `set_model`/`set_selected` emit `notify::selected` synchronously, but
    /// the handler used to defer through `sender.input`, so the echo arrived
    /// after the rebuild had lowered its suppression flag and looked like a
    /// deliberate move back to Local. Because every location switch re-emits
    /// `SetLocations`, that echo re-armed itself: pointing Files at a remote
    /// host (manually, or automatically when Files follows an SSH session)
    /// spun the GTK thread at 100% and froze the whole window.
    #[test]
    #[ignore = "requires DISPLAY"]
    fn programmatic_location_rebuild_does_not_echo_back_as_a_user_switch() {
        gtk::init().expect("GTK display");
        let labels = || vec!["Local".to_string(), "ssh: staging".to_string()];
        let details = || {
            vec![
                "Local filesystem".to_string(),
                "ssh: staging — deploy@server.example.com".to_string(),
            ]
        };
        let switches: Rc<std::cell::RefCell<Vec<usize>>> = Rc::default();
        let sink = switches.clone();
        let header = FileHeaderModel::builder()
            .launch((labels(), details()))
            .connect_receiver(move |_, output| {
                if let FileHeaderOutput::SelectLocation(index) = output {
                    sink.borrow_mut().push(index);
                }
            });
        while gtk::glib::MainContext::default().iteration(false) {}
        switches.borrow_mut().clear();

        header.emit(FileHeaderMsg::SetLocations {
            labels: labels(),
            details: details(),
            selected: 1,
        });
        while gtk::glib::MainContext::default().iteration(false) {}
        assert!(
            switches.borrow().is_empty(),
            "rebuilding the selector emitted {:?}",
            switches.borrow()
        );

        // The suppression is scoped to the rebuild: moving the selector still
        // switches the tree.
        let root = header.widget().clone().upcast::<gtk::Widget>();
        let location = descendant_named(&root, "file-tree-location")
            .expect("location dropdown in file header")
            .downcast::<gtk::DropDown>()
            .expect("named widget is a dropdown");
        location.set_selected(0);
        while gtk::glib::MainContext::default().iteration(false) {}
        assert_eq!(&*switches.borrow(), &[0]);
    }

    #[test]
    #[ignore = "requires DISPLAY"]
    fn file_header_terminal_button_is_focusable_and_updates_remote_semantics() {
        gtk::init().expect("GTK display");
        let header = FileHeaderModel::builder().launch((
            vec!["Local".to_string(), "ssh: staging".to_string()],
            vec![
                "Local filesystem".to_string(),
                "ssh: staging — deploy@server.example.com".to_string(),
            ],
        ));
        let root = header.widget().clone().upcast::<gtk::Widget>();
        let location = descendant_named(&root, "file-tree-location")
            .expect("location dropdown in file header")
            .downcast::<gtk::DropDown>()
            .expect("named widget is a dropdown");
        let terminal = descendant_named(&root, "file-tree-open-terminal")
            .expect("terminal button in file header")
            .downcast::<gtk::Button>()
            .expect("named widget is a button");

        assert!(terminal.is_focusable());
        assert_eq!(
            terminal.icon_name().as_deref(),
            Some("utilities-terminal-symbolic")
        );
        assert_eq!(
            terminal.tooltip_text().as_deref(),
            Some(LOCAL_TERMINAL_TOOLTIP)
        );
        assert_eq!(location.tooltip_text().as_deref(), Some("Local filesystem"));

        header.emit(FileHeaderMsg::SetLocations {
            labels: vec!["Local".to_string(), "ssh: staging".to_string()],
            details: vec![
                "Local filesystem".to_string(),
                "ssh: staging — deploy@server.example.com".to_string(),
            ],
            selected: 1,
        });
        while gtk::glib::MainContext::default().iteration(false) {}
        assert_eq!(
            terminal.tooltip_text().as_deref(),
            Some(REMOTE_TERMINAL_TOOLTIP)
        );
        assert_eq!(
            location.tooltip_text().as_deref(),
            Some("ssh: staging — deploy@server.example.com")
        );
    }
}
