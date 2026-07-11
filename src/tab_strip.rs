//! Relm4 factory row for the tab strip.

use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender};
use relm4::gtk;
use relm4::gtk::prelude::*;
use std::cell::Cell;
use std::rc::Rc;

use crate::{MAX_TAB_WIDTH, MIN_TAB_WIDTH};

#[derive(Debug, Clone, Copy)]
pub(crate) enum ConnectionState {
    Connecting,
    Connected,
    Disconnected,
}

#[derive(Debug)]
pub(crate) struct TabRowInit {
    pub(crate) id: u64,
    pub(crate) target_index: usize,
    pub(crate) title: String,
    pub(crate) active: bool,
    pub(crate) bell: bool,
    pub(crate) activity: bool,
    pub(crate) marked: bool,
    pub(crate) pinned: bool,
    pub(crate) connection: Option<ConnectionState>,
    pub(crate) remote_hosts: Vec<(u8, String)>,
    pub(crate) tab_width: u32,
    pub(crate) sidebar: bool,
}

#[derive(Debug)]
pub(crate) enum TabRowMsg {
    SetTitle(String),
}

#[derive(Debug)]
pub(crate) enum TabRowOutput {
    Select(u64),
    Close(u64),
    Rename(u64, String),
    NewTab,
    Action(u64, TabAction),
    ConnectRemote(u8),
    Resize(u32),
    Reorder { source_id: u64, target: usize },
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum TabAction {
    Duplicate,
    ToggleMarked,
    TogglePinned,
}

pub(crate) struct TabRow {
    pub(crate) id: u64,
    target_index: usize,
    title: String,
    active: bool,
    bell: bool,
    activity: bool,
    marked: bool,
    pinned: bool,
    connection: Option<ConnectionState>,
    remote_hosts: Vec<(u8, String)>,
    tab_width: u32,
    sidebar: bool,
}

#[relm4::factory(pub(crate))]
impl FactoryComponent for TabRow {
    type Init = TabRowInit;
    type Input = TabRowMsg;
    type Output = TabRowOutput;
    type CommandOutput = ();
    type ParentWidget = gtk::Box;

    view! {
        root = gtk::Box {
            set_orientation: gtk::Orientation::Horizontal,
            set_hexpand: self.sidebar,
            set_css_classes: &self.row_classes(),

            gtk::Label {
                set_label: "\u{25CF}",
                set_visible: self.connection.is_some(),
                set_css_classes: &self.connection_classes(),
            },

            #[name(select_button)]
            gtk::ToggleButton {
                set_widget_name: &format!("tab-{}", self.id),
                set_active: self.active,
                set_hexpand: self.sidebar,
                set_width_request: -1,
                set_css_classes: &self.button_classes(),
                #[watch]
                set_tooltip_text: Some(&self.title),
                connect_clicked[sender, id = self.id] => move |_| {
                    let _ = sender.output(TabRowOutput::Select(id));
                },

                gtk::Box {
                    set_orientation: gtk::Orientation::Horizontal,
                    set_spacing: 4,

                    gtk::Label {
                        #[watch]
                        set_label: &self.title,
                        set_ellipsize: gtk::pango::EllipsizeMode::End,
                        set_single_line_mode: true,
                        set_xalign: 0.0,
                        set_hexpand: true,
                    },

                    #[name(close_icon)]
                    gtk::Image {
                        set_icon_name: Some("window-close-symbolic"),
                        add_css_class: "tab-strip-close",
                        set_opacity: 0.0,
                    },
                },

                add_controller = gtk::EventControllerMotion {
                    connect_enter[close_icon] => move |_, _, _| close_icon.set_opacity(1.0),
                    connect_leave[close_icon] => move |_| close_icon.set_opacity(0.0),
                },

                add_controller = gtk::GestureClick {
                    set_propagation_phase: gtk::PropagationPhase::Capture,
                    connect_pressed[sender, select_button, close_icon, id = self.id] =>
                        move |gesture, _, x, y| {
                            if close_hit(&select_button, &close_icon, x, y) {
                                gesture.set_state(gtk::EventSequenceState::Claimed);
                                let _ = sender.output(TabRowOutput::Close(id));
                            }
                        },
                },

                add_controller = gtk::GestureClick {
                    set_button: gtk::gdk::BUTTON_PRIMARY,
                    connect_pressed[sender, select_button, id = self.id, title = self.title.clone()] =>
                        move |_, presses, _, _| {
                            if presses == 2 {
                                show_rename(&select_button, id, &title, sender.clone());
                            }
                        },
                },

                add_controller = gtk::GestureClick {
                    set_button: gtk::gdk::BUTTON_SECONDARY,
                    connect_pressed[
                        sender,
                        select_button,
                        id = self.id,
                        title = self.title.clone(),
                        marked = self.marked,
                        pinned = self.pinned,
                        remote_hosts = self.remote_hosts.clone()
                    ] => move |gesture, _, x, y| {
                        gesture.set_state(gtk::EventSequenceState::Claimed);
                        show_context_menu(
                            &select_button,
                            x,
                            y,
                            id,
                            &title,
                            marked,
                            pinned,
                            &remote_hosts,
                            sender.clone(),
                        );
                    },
                },

                add_controller = gtk::DragSource {
                    set_actions: gtk::gdk::DragAction::MOVE,
                    connect_prepare[id = self.id] => move |_, _, _| {
                        Some(gtk::gdk::ContentProvider::for_value(&id.to_value()))
                    },
                },
            },

            gtk::Box {
                set_orientation: gtk::Orientation::Vertical,
                set_visible: false,
                add_css_class: "tab-resize-handle",
                set_tooltip_text: Some("Drag to resize tabs"),

                add_controller = gtk::GestureDrag {
                    set_button: gtk::gdk::BUTTON_PRIMARY,
                    connect_drag_begin[start_width, tab_width = self.tab_width] => move |_, _, _| {
                        start_width.set(tab_width as i32);
                    },
                    connect_drag_update[select_button, start_width] => move |_, dx, _| {
                        let width = (start_width.get() + dx as i32)
                            .clamp(MIN_TAB_WIDTH as i32, MAX_TAB_WIDTH as i32);
                        select_button.set_width_request(width);
                    },
                    connect_drag_end[sender, start_width] => move |_, dx, _| {
                        let width = (start_width.get() + dx as i32)
                            .clamp(MIN_TAB_WIDTH as i32, MAX_TAB_WIDTH as i32)
                            as u32;
                        let _ = sender.output(TabRowOutput::Resize(width));
                    },
                },
            },

            add_controller = gtk::DropTarget::new(
                gtk::glib::Type::U64,
                gtk::gdk::DragAction::MOVE,
            ) {
                connect_drop[sender, target = self.target_index] => move |_, value, _, _| {
                    if let Ok(source_id) = value.get::<u64>() {
                        let _ = sender.output(TabRowOutput::Reorder { source_id, target });
                        true
                    } else {
                        false
                    }
                },
            },
        }
    }

    fn init_model(init: Self::Init, _index: &DynamicIndex, _sender: FactorySender<Self>) -> Self {
        Self {
            id: init.id,
            target_index: init.target_index,
            title: init.title,
            active: init.active,
            bell: init.bell,
            activity: init.activity,
            marked: init.marked,
            pinned: init.pinned,
            connection: init.connection,
            remote_hosts: init.remote_hosts,
            tab_width: init.tab_width,
            sidebar: init.sidebar,
        }
    }

    fn init_widgets(
        &mut self,
        _index: &Self::Index,
        root: Self::Root,
        _returned_widget: &<Self::ParentWidget as relm4::factory::FactoryView>::ReturnedWidget,
        sender: FactorySender<Self>,
    ) -> Self::Widgets {
        let start_width = Rc::new(Cell::new(self.tab_width as i32));
        let widgets = view_output!();
        widgets
    }

    fn update(&mut self, msg: Self::Input, _sender: FactorySender<Self>) {
        match msg {
            TabRowMsg::SetTitle(title) => self.title = title,
        }
    }
}

impl TabRow {
    fn row_classes(&self) -> Vec<&'static str> {
        let mut classes = vec!["tab-row"];
        if self.active {
            classes.push("active-tab");
        }
        classes
    }

    fn button_classes(&self) -> Vec<&'static str> {
        let mut classes = vec!["tab-strip-btn"];
        if self.bell {
            classes.push("tab-bell");
        }
        if self.activity {
            classes.push("tab-activity");
        }
        if self.marked {
            classes.push("tab-marked");
        }
        if self.pinned {
            classes.push("tab-pinned");
        }
        classes
    }

    fn connection_classes(&self) -> Vec<&'static str> {
        let state = match self.connection {
            Some(ConnectionState::Connecting) => "conn-connecting",
            Some(ConnectionState::Connected) => "conn-connected",
            Some(ConnectionState::Disconnected) => "conn-disconnected",
            None => "conn-disconnected",
        };
        vec!["conn-dot", state]
    }
}

fn close_hit(button: &gtk::ToggleButton, icon: &gtk::Image, x: f64, y: f64) -> bool {
    let point = gtk::graphene::Point::new(x as f32, y as f32);
    button
        .compute_point(icon, &point)
        .map(|mapped| {
            let x = mapped.x() as f64;
            let y = mapped.y() as f64;
            x >= 0.0 && y >= 0.0 && x <= icon.width() as f64 && y <= icon.height() as f64
        })
        .unwrap_or(false)
}

fn show_rename(button: &gtk::ToggleButton, id: u64, title: &str, sender: FactorySender<TabRow>) {
    let popover = gtk::Popover::new();
    popover.set_parent(button);
    let entry = gtk::Entry::new();
    entry.set_text(title);
    entry.select_region(0, -1);
    popover.set_child(Some(&entry));
    let popover_for_entry = popover.clone();
    entry.connect_activate(move |entry| {
        let _ = sender.output(TabRowOutput::Rename(id, entry.text().to_string()));
        popover_for_entry.popdown();
    });
    popover.connect_closed(|popover| popover.unparent());
    popover.popup();
    entry.grab_focus();
}

#[allow(clippy::too_many_arguments)]
fn show_context_menu(
    button: &gtk::ToggleButton,
    x: f64,
    y: f64,
    id: u64,
    title: &str,
    marked: bool,
    pinned: bool,
    remote_hosts: &[(u8, String)],
    sender: FactorySender<TabRow>,
) {
    let popover = gtk::Popover::new();
    popover.set_parent(button);
    popover.set_has_arrow(false);
    popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    let menu = gtk::Box::new(gtk::Orientation::Vertical, 0);
    menu.add_css_class("menu");

    add_menu_item(&menu, "New Tab", &popover, sender.clone(), move |sender| {
        sender.output(TabRowOutput::NewTab)
    });
    add_menu_item(
        &menu,
        "Duplicate",
        &popover,
        sender.clone(),
        move |sender| sender.output(TabRowOutput::Action(id, TabAction::Duplicate)),
    );
    add_menu_item(
        &menu,
        if marked { "Unmark" } else { "Mark Important" },
        &popover,
        sender.clone(),
        move |sender| sender.output(TabRowOutput::Action(id, TabAction::ToggleMarked)),
    );
    add_menu_item(
        &menu,
        if pinned { "Unpin Tab" } else { "Pin Tab" },
        &popover,
        sender.clone(),
        move |sender| sender.output(TabRowOutput::Action(id, TabAction::TogglePinned)),
    );

    let rename_button = menu_button("Rename");
    {
        let popover = popover.clone();
        let tab_button = button.clone();
        let title = title.to_string();
        let sender = sender.clone();
        rename_button.connect_clicked(move |_| {
            popover.popdown();
            show_rename(&tab_button, id, &title, sender.clone());
        });
    }
    menu.append(&rename_button);

    add_menu_item(&menu, "Close", &popover, sender.clone(), move |sender| {
        sender.output(TabRowOutput::Close(id))
    });

    if !remote_hosts.is_empty() {
        menu.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        for (index, name) in remote_hosts {
            let index = *index;
            add_menu_item(
                &menu,
                &format!("Remote: {name}"),
                &popover,
                sender.clone(),
                move |sender| sender.output(TabRowOutput::ConnectRemote(index)),
            );
        }
    }

    popover.set_child(Some(&menu));
    popover.connect_closed(|popover| popover.unparent());
    popover.popup();
}

fn menu_button(label: &str) -> gtk::Button {
    let button = gtk::Button::with_label(label);
    button.set_has_frame(false);
    button.add_css_class("flat");
    if let Some(child) = button.child() {
        child.set_halign(gtk::Align::Start);
    }
    button
}

fn add_menu_item<F>(
    menu: &gtk::Box,
    label: &str,
    popover: &gtk::Popover,
    sender: FactorySender<TabRow>,
    output: F,
) where
    F: Fn(&FactorySender<TabRow>) -> Result<(), TabRowOutput> + 'static,
{
    let button = menu_button(label);
    let popover = popover.clone();
    button.connect_clicked(move |_| {
        popover.popdown();
        let _ = output(&sender);
    });
    menu.append(&button);
}
