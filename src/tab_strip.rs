//! Relm4 factory row for the tab strip.

use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender};
use relm4::gtk;
use relm4::gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use crate::pane_header::{WorkspaceDragItem, WorkspaceDragPayload};
use crate::{MAX_TAB_WIDTH, MIN_TAB_WIDTH};

const TAB_DROP_PREVIEW_DELAY: Duration = Duration::from_millis(450);

/// One process-wide identity boundary for tab drags and delayed hover
/// previews. Factory rows can be rebuilt while GTK still owns an old timeout,
/// so neither a row-local counter nor a source id alone is sufficient.
#[derive(Debug)]
pub(crate) struct TabDragCoordinator {
    next_drag_id: Cell<Option<u64>>,
    active: Cell<Option<(u64, u64)>>,
    hover_generation: Cell<Option<u64>>,
}

impl Default for TabDragCoordinator {
    fn default() -> Self {
        Self {
            next_drag_id: Cell::new(Some(0)),
            active: Cell::new(None),
            hover_generation: Cell::new(Some(0)),
        }
    }
}

impl TabDragCoordinator {
    fn advance(cell: &Cell<Option<u64>>) -> Option<u64> {
        let next = cell.get()?.checked_add(1);
        cell.set(next);
        next
    }

    fn clear_active_hover(&self) {
        self.active.set(None);
        self.hover_generation.set(None);
    }

    fn advance_hover_or_fail(&self) -> Option<u64> {
        let next = Self::advance(&self.hover_generation);
        if next.is_none() {
            self.clear_active_hover();
        }
        next
    }

    fn begin(&self, source_tab_id: u64) -> Option<u64> {
        let Some(drag_id) = Self::advance(&self.next_drag_id) else {
            self.clear_active_hover();
            return None;
        };
        self.active.set(Some((source_tab_id, drag_id)));
        self.advance_hover_or_fail()?;
        Some(drag_id)
    }

    fn finish(&self, source_tab_id: u64, drag_id: u64) {
        if self.active.get() == Some((source_tab_id, drag_id)) {
            self.active.set(None);
            self.invalidate_hover();
        }
    }

    fn drag_id_for(&self, source_tab_id: u64) -> Option<u64> {
        self.active
            .get()
            .filter(|(source, _)| *source == source_tab_id)
            .map(|(_, drag_id)| drag_id)
    }

    pub(crate) fn drag_is_current(&self, source_tab_id: u64, drag_id: u64) -> bool {
        self.active.get() == Some((source_tab_id, drag_id))
    }

    fn begin_hover(&self, source_tab_id: u64, drag_id: u64) -> Option<u64> {
        (self.active.get() == Some((source_tab_id, drag_id)))
            .then(|| self.advance_hover_or_fail())
            .flatten()
    }

    fn cancel_hover(&self, source_tab_id: u64, drag_id: u64) {
        if self.active.get() == Some((source_tab_id, drag_id)) {
            self.invalidate_hover();
        }
    }

    pub(crate) fn invalidate_hover(&self) {
        let _ = self.advance_hover_or_fail();
    }

    pub(crate) fn hover_is_current(
        &self,
        source_tab_id: u64,
        drag_id: u64,
        hover_generation: u64,
    ) -> bool {
        self.active.get() == Some((source_tab_id, drag_id))
            && self.hover_generation.get() == Some(hover_generation)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectionState {
    Connecting,
    Connected,
    Disconnected,
}

#[derive(Debug, Clone)]
pub(crate) struct TabRowInit {
    pub(crate) id: u64,
    pub(crate) target_index: usize,
    pub(crate) title: String,
    pub(crate) real_title: String,
    pub(crate) active: bool,
    pub(crate) bell: bool,
    pub(crate) activity: bool,
    pub(crate) marked: bool,
    pub(crate) pinned: bool,
    pub(crate) private_title: bool,
    pub(crate) connection: Option<ConnectionState>,
    pub(crate) remote_hosts: Vec<(u8, String)>,
    pub(crate) tab_width: u32,
    pub(crate) sidebar: bool,
    pub(crate) drag_coordinator: Rc<TabDragCoordinator>,
}

#[derive(Debug)]
pub(crate) enum TabRowMsg {
    SetTitles { title: String, real_title: String },
    Sync(TabRowInit),
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
    Reorder {
        source_id: u64,
        target: usize,
    },
    DragStarted {
        source_tab_id: u64,
        drag_id: u64,
    },
    DragEnded {
        source_tab_id: u64,
        drag_id: u64,
    },
    PreviewDropTarget {
        source_tab_id: u64,
        target_tab_id: u64,
        drag_id: u64,
        hover_generation: u64,
    },
    PromotePane {
        pane_id: u64,
        anchor_tab_id: u64,
        after: bool,
    },
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum TabAction {
    Duplicate,
    ToggleMarked,
    TogglePinned,
    TogglePrivateTitle,
}

pub(crate) struct TabRow {
    pub(crate) id: u64,
    target_index: usize,
    title: String,
    real_title: String,
    active: bool,
    bell: bool,
    activity: bool,
    marked: bool,
    pinned: bool,
    private_title: bool,
    connection: Option<ConnectionState>,
    remote_hosts: Vec<(u8, String)>,
    tab_width: u32,
    sidebar: bool,
    action_state: Rc<RefCell<TabRowActionState>>,
    drag_coordinator: Rc<TabDragCoordinator>,
}

#[derive(Debug, Clone)]
struct TabRowActionState {
    target_index: usize,
    real_title: String,
    marked: bool,
    pinned: bool,
    private_title: bool,
    remote_hosts: Vec<(u8, String)>,
    tab_width: u32,
    sidebar: bool,
}

impl TabRowActionState {
    fn from_init(init: &TabRowInit) -> Self {
        Self {
            target_index: init.target_index,
            real_title: init.real_title.clone(),
            marked: init.marked,
            pinned: init.pinned,
            private_title: init.private_title,
            remote_hosts: init.remote_hosts.clone(),
            tab_width: init.tab_width,
            sidebar: init.sidebar,
        }
    }
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
            #[watch]
            set_hexpand: self.sidebar,
            #[watch]
            set_css_classes: &self.row_classes(),

            #[name(select_button)]
            gtk::ToggleButton {
                set_widget_name: &format!("tab-{}", self.id),
                #[watch]
                set_active: self.active,
                #[watch]
                set_hexpand: self.sidebar,
                #[watch]
                set_width_request: if self.sidebar { -1 } else { self.tab_width as i32 },
                #[watch]
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
                        set_label: "\u{25CF}",
                        #[watch]
                        set_visible: self.connection.is_some(),
                        #[watch]
                        set_css_classes: &self.connection_classes(),
                    },

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

                    gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        #[watch]
                        set_visible: !self.sidebar,
                        add_css_class: "tab-resize-handle",
                        set_cursor_from_name: Some("col-resize"),
                        set_tooltip_text: Some("Drag to resize tabs"),
                        update_property: &[
                            gtk::accessible::Property::Label("Resize tabs"),
                        ],

                        add_controller = gtk::GestureDrag {
                            set_button: gtk::gdk::BUTTON_PRIMARY,
                            set_propagation_phase: gtk::PropagationPhase::Capture,
                            connect_drag_begin[
                                start_width,
                                action_state = self.action_state.clone()
                            ] => move |gesture, _, _| {
                                gesture.set_state(gtk::EventSequenceState::Claimed);
                                start_width.set(action_state.borrow().tab_width as i32);
                            },
                            connect_drag_update[select_button, start_width] => move |gesture, dx, _| {
                                gesture.set_state(gtk::EventSequenceState::Claimed);
                                let width = (start_width.get() + dx as i32)
                                    .clamp(MIN_TAB_WIDTH as i32, MAX_TAB_WIDTH as i32);
                                select_button.set_width_request(width);
                            },
                            connect_drag_end[sender, start_width] => move |gesture, dx, _| {
                                gesture.set_state(gtk::EventSequenceState::Claimed);
                                let width = (start_width.get() + dx as i32)
                                    .clamp(MIN_TAB_WIDTH as i32, MAX_TAB_WIDTH as i32)
                                    as u32;
                                let _ = sender.output(TabRowOutput::Resize(width));
                            },
                        },
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
                    connect_pressed[
                        sender,
                        select_button,
                        id = self.id,
                        action_state = self.action_state.clone()
                    ] =>
                        move |_, presses, _, _| {
                            if presses == 2 {
                                let title = action_state.borrow().real_title.clone();
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
                        action_state = self.action_state.clone()
                    ] => move |gesture, _, x, y| {
                        gesture.set_state(gtk::EventSequenceState::Claimed);
                        let state = action_state.borrow().clone();
                        show_context_menu(
                            &select_button,
                            x,
                            y,
                            id,
                            &state.real_title,
                            state.marked,
                            state.pinned,
                            state.private_title,
                            &state.remote_hosts,
                            sender.clone(),
                        );
                    },
                },

                add_controller = gtk::DragSource {
                    set_actions: gtk::gdk::DragAction::MOVE,
                    connect_prepare[id = self.id] => move |_, _, _| {
                        let payload = WorkspaceDragPayload::tab(id);
                        Some(gtk::gdk::ContentProvider::for_value(&payload.to_value()))
                    },
                    connect_drag_begin[
                        sender,
                        row_drag_id,
                        drag_coordinator = self.drag_coordinator.clone(),
                        id = self.id
                    ] => move |_, _| {
                        let Some(drag_id) = drag_coordinator.begin(id) else {
                            return;
                        };
                        row_drag_id.set(Some(drag_id));
                        let _ = sender.output(TabRowOutput::DragStarted {
                            source_tab_id: id,
                            drag_id,
                        });
                    },
                    connect_drag_end[
                        sender,
                        row_drag_id,
                        drag_coordinator = self.drag_coordinator.clone(),
                        id = self.id
                    ] => move |_, _, _| {
                        let Some(drag_id) = row_drag_id.take() else {
                            return;
                        };
                        drag_coordinator.finish(id, drag_id);
                        let _ = sender.output(TabRowOutput::DragEnded {
                            source_tab_id: id,
                            drag_id,
                        });
                    },
                },
            },

            add_controller = gtk::DropTarget::new(
                WorkspaceDragPayload::static_type(),
                gtk::gdk::DragAction::MOVE,
            ) {
                set_preload: true,
                connect_enter[
                    sender,
                    select_button,
                    hover_drag,
                    drag_coordinator = self.drag_coordinator.clone(),
                    id = self.id
                ] => move |target, _, _| {
                    if let Some((source_tab_id, drag_id)) = hover_drag.take() {
                        drag_coordinator.cancel_hover(source_tab_id, drag_id);
                    }
                    let Some(item) = target
                        .value()
                        .and_then(|value| value.get::<WorkspaceDragPayload>().ok())
                        .map(|payload| payload.item())
                    else {
                        return gtk::gdk::DragAction::empty();
                    };
                    match item {
                        WorkspaceDragItem::Pane(_) => {
                            select_button.add_css_class("pane-to-tab-drop");
                        }
                        WorkspaceDragItem::Tab(_) => {
                            let Some((source_tab_id, target_tab_id)) =
                                tab_drop_preview(item, id)
                            else {
                                return gtk::gdk::DragAction::empty();
                            };
                            let Some(drag_id) = drag_coordinator.drag_id_for(source_tab_id) else {
                                return gtk::gdk::DragAction::empty();
                            };
                            let Some(hover_generation) =
                                drag_coordinator.begin_hover(source_tab_id, drag_id)
                            else {
                                return gtk::gdk::DragAction::empty();
                            };
                            hover_drag.set(Some((source_tab_id, drag_id)));
                            select_button.add_css_class("pane-to-tab-drop");
                            let weak_button = select_button.downgrade();
                            let drag_coordinator = drag_coordinator.clone();
                            let sender = sender.clone();
                            gtk::glib::timeout_add_local_once(
                                TAB_DROP_PREVIEW_DELAY,
                                move || {
                                    let button_is_live = weak_button.upgrade().is_some_and(|button| {
                                        button.is_mapped()
                                            && button.has_css_class("pane-to-tab-drop")
                                    });
                                    if button_is_live
                                        && drag_coordinator.hover_is_current(
                                            source_tab_id,
                                            drag_id,
                                            hover_generation,
                                        )
                                    {
                                        let _ = sender.output(
                                            TabRowOutput::PreviewDropTarget {
                                                source_tab_id,
                                                target_tab_id,
                                                drag_id,
                                                hover_generation,
                                            },
                                        );
                                    }
                                },
                            );
                        }
                    }
                    gtk::gdk::DragAction::MOVE
                },
                connect_leave[
                    select_button,
                    hover_drag,
                    drag_coordinator = self.drag_coordinator.clone()
                ] => move |_| {
                    select_button.remove_css_class("pane-to-tab-drop");
                    if let Some((source_tab_id, drag_id)) = hover_drag.take() {
                        drag_coordinator.cancel_hover(source_tab_id, drag_id);
                    }
                },
                connect_drop[
                    sender,
                    select_button,
                    hover_drag,
                    drag_coordinator = self.drag_coordinator.clone(),
                    id = self.id,
                    action_state = self.action_state.clone()
                ] => move |_, value, x, y| {
                    select_button.remove_css_class("pane-to-tab-drop");
                    if let Some((source_tab_id, drag_id)) = hover_drag.take() {
                        drag_coordinator.cancel_hover(source_tab_id, drag_id);
                    }
                    let Ok(payload) = value.get::<WorkspaceDragPayload>() else {
                        return false;
                    };
                    match payload.item() {
                        WorkspaceDragItem::Tab(source_id) => {
                            if source_id == id {
                                return false;
                            }
                            let target = action_state.borrow().target_index;
                            let _ = sender.output(TabRowOutput::Reorder { source_id, target });
                            true
                        }
                        WorkspaceDragItem::Pane(pane_id) => {
                            let state = action_state.borrow();
                            let after = if state.sidebar {
                                y >= f64::from(select_button.height()) / 2.0
                            } else {
                                x >= f64::from(select_button.width()) / 2.0
                            };
                            let _ = sender.output(TabRowOutput::PromotePane {
                                pane_id,
                                anchor_tab_id: id,
                                after,
                            });
                            true
                        }
                    }
                },
            },
        }
    }

    fn init_model(init: Self::Init, _index: &DynamicIndex, _sender: FactorySender<Self>) -> Self {
        Self::from_init(init)
    }

    fn init_widgets(
        &mut self,
        _index: &Self::Index,
        root: Self::Root,
        _returned_widget: &<Self::ParentWidget as relm4::factory::FactoryView>::ReturnedWidget,
        sender: FactorySender<Self>,
    ) -> Self::Widgets {
        let start_width = Rc::new(Cell::new(self.tab_width as i32));
        let row_drag_id = Rc::new(Cell::new(None::<u64>));
        let hover_drag = Rc::new(Cell::new(None::<(u64, u64)>));
        let widgets = view_output!();
        widgets
    }

    fn update(&mut self, msg: Self::Input, _sender: FactorySender<Self>) {
        match msg {
            TabRowMsg::SetTitles { title, real_title } => {
                self.action_state.borrow_mut().real_title = real_title;
                self.title = title;
            }
            TabRowMsg::Sync(init) => self.sync_from(init),
        }
    }
}

fn tab_drop_preview(item: WorkspaceDragItem, target_tab_id: u64) -> Option<(u64, u64)> {
    match item {
        WorkspaceDragItem::Tab(source_tab_id) if source_tab_id != target_tab_id => {
            Some((source_tab_id, target_tab_id))
        }
        WorkspaceDragItem::Tab(_) | WorkspaceDragItem::Pane(_) => None,
    }
}

/// Accept a pane header drop on otherwise-empty tab-bar space. Row targets
/// provide an insertion anchor; this fallback promotes next to the source tab.
pub(crate) fn install_pane_drop_target(
    tab_bar: &gtk::Box,
    on_drop: impl Fn(u64) -> bool + 'static,
) {
    let target = gtk::DropTarget::new(
        WorkspaceDragPayload::static_type(),
        gtk::gdk::DragAction::MOVE,
    );
    target.connect_drop(move |_, value, _, _| {
        let Ok(payload) = value.get::<WorkspaceDragPayload>() else {
            return false;
        };
        match payload.item() {
            WorkspaceDragItem::Pane(pane_id) => on_drop(pane_id),
            WorkspaceDragItem::Tab(_) => false,
        }
    });
    tab_bar.add_controller(target);
}

impl TabRow {
    fn from_init(init: TabRowInit) -> Self {
        let action_state = Rc::new(RefCell::new(TabRowActionState::from_init(&init)));
        Self {
            id: init.id,
            target_index: init.target_index,
            title: init.title,
            real_title: init.real_title,
            active: init.active,
            bell: init.bell,
            activity: init.activity,
            marked: init.marked,
            pinned: init.pinned,
            private_title: init.private_title,
            connection: init.connection,
            remote_hosts: init.remote_hosts,
            tab_width: init.tab_width,
            sidebar: init.sidebar,
            action_state,
            drag_coordinator: init.drag_coordinator,
        }
    }

    pub(crate) fn matches_init(&self, init: &TabRowInit) -> bool {
        self.id == init.id
            && self.target_index == init.target_index
            && self.title == init.title
            && self.real_title == init.real_title
            && self.active == init.active
            && self.bell == init.bell
            && self.activity == init.activity
            && self.marked == init.marked
            && self.pinned == init.pinned
            && self.private_title == init.private_title
            && self.connection == init.connection
            && self.remote_hosts == init.remote_hosts
            && self.tab_width == init.tab_width
            && self.sidebar == init.sidebar
    }

    fn sync_from(&mut self, init: TabRowInit) {
        debug_assert_eq!(self.id, init.id);
        debug_assert!(Rc::ptr_eq(&self.drag_coordinator, &init.drag_coordinator));
        *self.action_state.borrow_mut() = TabRowActionState::from_init(&init);
        self.target_index = init.target_index;
        self.title = init.title;
        self.real_title = init.real_title;
        self.active = init.active;
        self.bell = init.bell;
        self.activity = init.activity;
        self.marked = init.marked;
        self.pinned = init.pinned;
        self.private_title = init.private_title;
        self.connection = init.connection;
        self.remote_hosts = init.remote_hosts;
        self.tab_width = init.tab_width;
        self.sidebar = init.sidebar;
    }

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
    private_title: bool,
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
    add_menu_item(
        &menu,
        if private_title {
            "Show Title Details"
        } else {
            "Hide Title Details"
        },
        &popover,
        sender.clone(),
        move |sender| sender.output(TabRowOutput::Action(id, TabAction::TogglePrivateTitle)),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn init() -> TabRowInit {
        TabRowInit {
            id: 7,
            target_index: 2,
            title: "shell".to_string(),
            real_title: "shell".to_string(),
            active: false,
            bell: false,
            activity: false,
            marked: false,
            pinned: false,
            private_title: false,
            connection: Some(ConnectionState::Connecting),
            remote_hosts: vec![(0, "host-a".to_string())],
            tab_width: 180,
            sidebar: true,
            drag_coordinator: Rc::new(TabDragCoordinator::default()),
        }
    }

    #[test]
    fn sync_updates_visual_and_action_state_without_replacing_identity() {
        let mut row = TabRow::from_init(init());
        let mut updated = init();
        updated.drag_coordinator = row.drag_coordinator.clone();
        updated.target_index = 4;
        updated.title = "remote".to_string();
        updated.real_title = "remote".to_string();
        updated.active = true;
        updated.bell = true;
        updated.marked = true;
        updated.connection = Some(ConnectionState::Connected);
        updated.remote_hosts.push((1, "host-b".to_string()));
        updated.tab_width = 240;
        updated.sidebar = false;

        row.sync_from(updated.clone());

        assert!(row.matches_init(&updated));
        let actions = row.action_state.borrow();
        assert_eq!(actions.target_index, 4);
        assert_eq!(actions.real_title, "remote");
        assert!(actions.marked);
        assert_eq!(actions.remote_hosts.len(), 2);
        assert_eq!(actions.tab_width, 240);
    }

    #[test]
    fn hover_preview_uses_stable_tab_ids_and_excludes_self_or_pane_drags() {
        assert_eq!(
            tab_drop_preview(WorkspaceDragItem::Tab(71), 93),
            Some((71, 93))
        );
        assert_eq!(tab_drop_preview(WorkspaceDragItem::Tab(71), 71), None);
        assert_eq!(tab_drop_preview(WorkspaceDragItem::Pane(71), 93), None);
    }

    #[test]
    fn global_drag_identity_rejects_stale_hover_and_stale_end() {
        let coordinator = TabDragCoordinator::default();
        let first = coordinator.begin(7).unwrap();
        let old_hover = coordinator.begin_hover(7, first).unwrap();
        assert!(coordinator.hover_is_current(7, first, old_hover));

        coordinator.finish(7, first);
        let second = coordinator.begin(7).unwrap();
        let new_hover = coordinator.begin_hover(7, second).unwrap();
        coordinator.finish(7, first);

        assert!(!coordinator.hover_is_current(7, first, old_hover));
        assert!(coordinator.hover_is_current(7, second, new_hover));
    }

    #[test]
    fn generation_overflow_clears_every_stale_drag_authority() {
        let coordinator = TabDragCoordinator::default();
        coordinator.next_drag_id.set(Some(u64::MAX));
        coordinator.active.set(Some((7, 41)));
        coordinator.hover_generation.set(Some(9));

        assert_eq!(coordinator.begin(8), None);
        assert_eq!(coordinator.active.get(), None);
        assert_eq!(coordinator.hover_generation.get(), None);

        let coordinator = TabDragCoordinator::default();
        coordinator.active.set(Some((7, 41)));
        coordinator.hover_generation.set(Some(u64::MAX));

        assert_eq!(coordinator.begin_hover(7, 41), None);
        assert_eq!(coordinator.active.get(), None);
        assert_eq!(coordinator.hover_generation.get(), None);
    }
}
