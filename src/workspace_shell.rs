//! Native presentation adapter for the shared four-terminal workspace contract.
//!
//! This module never reads or writes the PTY. Existing terminal widgets keep
//! their identity, input ownership, scroll anchors and lifecycle callbacks.
use crate::{
    app_msg::AppMsg,
    config::{Config, TerminalMode},
    keybindings::Action,
    AppModel,
};
use gtk::prelude::*;
use jterm_core::reference_shell as tokens;
use relm4::{gtk, ComponentSender};
use vte4::TerminalExt;

pub(crate) struct Chrome {
    pub top: gtk::Overlay,
    pub rail: gtk::Box,
    pub center: gtk::Box,
    pub canvas_header: gtk::Box,
    pub block_composer_chrome: gtk::Box,
    pub inspector: gtk::Box,
    pub status: gtk::Box,
    title: gtk::Label,
    path: gtk::Label,
    workspace_name: gtk::Label,
    mode: gtk::Label,
    block_mode: gtk::Button,
    chat_mode: gtk::Button,
    context_details: gtk::Label,
    block_shelf: gtk::Box,
    presentation_navigation: gtk::Label,
    search: gtk::Button,
}

fn label(text: &str, class: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.add_css_class(class);
    label
}

pub(crate) fn icon(kind: jterm_core::reference_icons::Icon) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.set_content_width(16);
    area.set_content_height(16);
    area.set_valign(gtk::Align::Center);
    area.set_draw_func(move |widget, cr, width, height| {
        let scale = width.min(height) as f64 / 24.0;
        cr.translate(
            (width as f64 - 24.0 * scale) / 2.0,
            (height as f64 - 24.0 * scale) / 2.0,
        );
        cr.scale(scale, scale);
        #[allow(deprecated)]
        let color = widget.style_context().color();
        cr.set_source_rgba(
            color.red() as f64,
            color.green() as f64,
            color.blue() as f64,
            color.alpha() as f64,
        );
        cr.set_line_width(1.5);
        cr.set_line_cap(gtk::cairo::LineCap::Round);
        cr.set_line_join(gtk::cairo::LineJoin::Round);
        for path in kind.paths() {
            if let Some((first, tail)) = path.split_first() {
                cr.move_to(first.0 as f64, first.1 as f64);
                for point in tail {
                    cr.line_to(point.0 as f64, point.1 as f64);
                }
                let _ = cr.stroke();
            }
        }
    });
    area
}

fn footer_icon(kind: jterm_core::reference_icons::Icon) -> gtk::DrawingArea {
    let icon = icon(kind);
    icon.set_content_width(jterm_core::reference_popup::FOOTER_ICON_SIZE as i32);
    icon.set_content_height(jterm_core::reference_popup::FOOTER_ICON_SIZE as i32);
    icon
}

fn product_icon(product: &str, size: i32) -> gtk::Image {
    thread_local! {
        static CACHE: std::cell::RefCell<std::collections::HashMap<String, gtk::gdk::Texture>> =
            std::cell::RefCell::new(std::collections::HashMap::new());
    }
    let texture = CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(texture) = cache.get(product) {
            return Some(texture.clone());
        }
        let bytes = jterm_core::reference_identity::png(product)?;
        let texture = gtk::gdk::Texture::from_bytes(&gtk::glib::Bytes::from_static(bytes)).ok()?;
        cache.insert(product.to_string(), texture.clone());
        Some(texture)
    });
    let image = gtk::Image::from_paintable(texture.as_ref());
    image.set_pixel_size(size);
    image
}

fn icon_for_name(name: &str) -> jterm_core::reference_icons::Icon {
    use jterm_core::reference_icons::Icon;
    if name == "workspace" {
        Icon::Grid
    } else if name == "agent" {
        Icon::Agent
    } else if name.contains("conversation") {
        Icon::Conversation
    } else if name.contains("terminal") {
        Icon::Terminal
    } else if name.contains("folder") {
        Icon::Files
    } else if name.contains("search") {
        Icon::Search
    } else if name.contains("settings") || name.contains("preferences") {
        Icon::Settings
    } else if name.contains("list-add") {
        Icon::Plus
    } else if name.contains("recent") {
        Icon::History
    } else if name.contains("system-run") {
        Icon::Agent
    } else if name.contains("more") {
        Icon::More
    } else if name.contains("close") {
        Icon::Close
    } else {
        Icon::Grid
    }
}

fn button(icon: &str, title: &str, sender: &ComponentSender<AppModel>, msg: AppMsg) -> gtk::Button {
    let button = gtk::Button::new();
    if icon.contains("window-") {
        button.set_icon_name(icon);
    } else {
        button.set_child(Some(&self::icon(icon_for_name(icon))));
    }
    button.add_css_class("flat");
    button.set_focus_on_click(false);
    button.set_tooltip_text(Some(title));
    button.update_property(&[gtk::accessible::Property::Label(title)]);
    let sender = sender.clone();
    button.connect_clicked(move |_| sender.input(msg.clone()));
    button
}

fn action_row(
    icon: &str,
    text: &str,
    sender: &ComponentSender<AppModel>,
    msg: AppMsg,
) -> gtk::Button {
    let row = button(icon, text, sender, msg);
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 9);
    content.append(&self::icon(icon_for_name(icon)));
    let title = label(text, "shell-navigation-title");
    title.set_hexpand(true);
    content.append(&title);
    row.set_child(Some(&content));
    row.add_css_class("shell-navigation-row");
    // Row height and inter-row gap are separate shared measurements.
    row.set_height_request(tokens::NAVIGATION_ROW_HEIGHT as i32);
    row
}

fn unavailable(text: &str) -> gtk::Button {
    let button = gtk::Button::with_label(text);
    button.add_css_class("flat");
    button.set_sensitive(false);
    button.set_tooltip_text(Some("Not connected yet"));
    button
}

struct ComposerChromeRoot {
    widget: gtk::glib::WeakRef<gtk::Widget>,
    pointer: gtk::glib::WeakRef<gtk::EventControllerMotion>,
}

thread_local! {
    static COMPOSER_CHROME_ROOTS: std::cell::RefCell<Vec<ComposerChromeRoot>> = const {
        std::cell::RefCell::new(Vec::new())
    };
}

/// Only tool/status chrome opts in. The editable terminal and composer frame
/// remain Content-owned; weak entries cannot keep a closed pane alive.
pub(crate) fn own_composer_chrome(widget: &impl IsA<gtk::Widget>) {
    let widget = widget.as_ref();
    widget.add_css_class("reference-composer-chrome");
    widget.add_css_class("anvil-ui-font");
    let pointer = gtk::EventControllerMotion::new();
    pointer.set_propagation_phase(gtk::PropagationPhase::Capture);
    pointer.set_propagation_limit(gtk::PropagationLimit::SameNative);
    COMPOSER_CHROME_ROOTS.with_borrow_mut(|roots| {
        roots.retain(|root| root.widget.upgrade().is_some());
        roots.push(ComposerChromeRoot {
            widget: widget.downgrade(),
            pointer: pointer.downgrade(),
        });
    });
    widget.add_controller(pointer);
}

/// The ancestor Content capture handler delegates these mapped pointer-owned
/// leaves before consuming the wheel. Keyboard focus never chooses the group.
pub(crate) fn composer_chrome_owns_font_scroll(root: &gtk::Widget) -> bool {
    COMPOSER_CHROME_ROOTS.with_borrow(|roots| {
        roots.iter().any(|entry| {
            entry.widget.upgrade().is_some_and(|widget| {
                widget.is_mapped()
                    && widget.height() > 0
                    && (widget == *root || widget.is_ancestor(root))
                    && entry
                        .pointer
                        .upgrade()
                        .is_some_and(|pointer| pointer.contains_pointer())
            })
        })
    })
}

/// One native tool row for both public presentations. The caller retains
/// the only Send/Stop authority; Block supplies a disabled Send button.
pub(crate) fn composer_tools(
    send: &gtk::Button,
    stop: Option<&gtk::Button>,
    focus_terminal: std::rc::Rc<dyn Fn()>,
    workspace_action: std::rc::Rc<dyn Fn(Action)>,
) -> (gtk::Box, gtk::Label) {
    use jterm_core::reference_icons::Icon;
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let secondary = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let add = unavailable("Attach");
    add.add_css_class("chat-add");
    add.set_child(Some(&footer_icon(Icon::Plus)));
    secondary.append(&add);
    for name in ["Attach", "Image", "Code"] {
        let button = unavailable(name);
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        content.append(&footer_icon(Icon::from_key(&name.to_ascii_lowercase())));
        content.append(&gtk::Label::new(Some(name)));
        button.set_child(Some(&content));
        secondary.append(&button);
    }
    let terminal = gtk::Button::new();
    terminal.add_css_class("flat");
    terminal.add_css_class("chat-terminal-capsule");
    terminal.set_focus_on_click(false);
    let terminal_content = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    terminal_content.append(&footer_icon(Icon::Terminal));
    terminal_content.append(&label("Terminal", "shell-canvas-subtitle"));
    terminal.set_child(Some(&terminal_content));
    terminal.set_tooltip_text(Some("Focus the active terminal"));
    terminal.update_property(&[gtk::accessible::Property::Label("Focus terminal")]);
    let focus = focus_terminal.clone();
    terminal.connect_clicked(move |_| focus());
    secondary.append(&terminal);
    let secondary_scroll = gtk::ScrolledWindow::new();
    secondary_scroll.set_policy(gtk::PolicyType::External, gtk::PolicyType::Never);
    secondary_scroll.set_min_content_width(0);
    secondary_scroll.set_max_content_width(400);
    secondary_scroll.set_width_request(1);
    secondary_scroll.set_hexpand(false);
    secondary_scroll.set_propagate_natural_width(true);
    secondary_scroll.set_child(Some(&secondary));
    row.append(&secondary_scroll);
    let tools = gtk::MenuButton::new();
    tools.add_css_class("flat");
    tools.add_css_class("reference-tools-button");
    let tools_content = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    let tools_grid = footer_icon(Icon::Grid);
    tools_content.append(&tools_grid);
    let tools_label = label("Tools", "shell-tool-label");
    tools_label.set_width_chars(0);
    tools_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    tools_content.append(&tools_label);
    tools_content.append(&footer_icon(Icon::ChevronDown));
    tools.set_child(Some(&tools_content));
    tools.set_direction(gtk::ArrowType::Up);
    tools.set_always_show_arrow(false);
    tools.update_property(&[gtk::accessible::Property::Label("Tools")]);
    tools.set_tooltip_text(Some("Tools"));
    let popover = gtk::Popover::new();
    popover.add_css_class("reference-tools-popover");
    popover.set_widget_name("reference-tools-menu");
    popover.set_position(gtk::PositionType::Top);
    popover.set_has_arrow(false);
    let entries = gtk::Box::new(gtk::Orientation::Vertical, 0);
    entries.set_width_request(
        (jterm_core::reference_popup::WIDTH - 2.0 * jterm_core::reference_popup::PADDING - 2.0)
            as i32,
    );
    for command in jterm_core::reference_popup::COMMANDS {
        use jterm_core::reference_popup::ToolCommandId;
        if command.separator_before {
            let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
            separator.set_margin_top(4);
            separator.set_margin_bottom(4);
            entries.append(&separator);
        }
        let entry = gtk::Button::new();
        entry.add_css_class("reference-tool-row");
        entry.set_height_request(jterm_core::reference_popup::ROW_HEIGHT as i32);
        entry.set_sensitive(!command.placeholder);
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        content.append(&footer_icon(Icon::from_key(command.icon_key)));
        let title = label(command.title, "reference-tool-title");
        title.set_hexpand(true);
        content.append(&title);
        content.append(&label(command.description, "reference-tool-description"));
        entry.set_child(Some(&content));
        entry.update_property(&[gtk::accessible::Property::Label(command.title)]);
        if !command.placeholder {
            let weak_popover = popover.downgrade();
            let focus = focus_terminal.clone();
            let action = workspace_action.clone();
            let id = command.id;
            entry.connect_clicked(move |_| {
                if let Some(popover) = weak_popover.upgrade() {
                    popover.popdown();
                }
                log::debug!(target: "anvil::workspace_shell", "tools_action={id:?}");
                match id {
                    ToolCommandId::Terminal => focus(),
                    ToolCommandId::CommandPalette => action(Action::ToggleCommandPalette),
                    ToolCommandId::SearchCanvas => action(Action::ToggleSearch),
                    _ => {}
                }
            });
        }
        entries.append(&entry);
    }
    popover.set_child(Some(&entries));
    tools.set_popover(Some(&popover));
    let weak_tools = tools.downgrade();
    popover.connect_visible_notify(move |popover| {
        if !log::log_enabled!(target: "anvil::workspace_shell", log::Level::Debug) { return; }
        let Some(tools) = weak_tools.upgrade() else { return; };
        if let Some(window) = tools.root().and_downcast::<gtk::Window>() {
            let mut composer = tools.parent();
            while let Some(widget) = composer {
                if widget.has_css_class("chat-composer") || widget.has_css_class("shell-block-composer") {
                    log::debug!(target: "anvil::workspace_shell", "tools_frame visible={} window={}x{} tools={:?} composer={:?}", popover.is_visible(),window.width(),window.height(),tools.compute_bounds(&window),widget.compute_bounds(&window));
                    break;
                }
                composer = widget.parent();
            }
        }
        let mut current: gtk::Widget = tools.upcast();
        while let Some(parent) = current.parent() {
            current = parent;
            if current.has_css_class("workspace-center") { break; }
        }
        fn find_scroll(widget: &gtk::Widget) -> Option<gtk::ScrolledWindow> {
            if widget.has_css_class("block-scroll") {
                return widget.clone().downcast().ok();
            }
            let mut child = widget.first_child();
            while let Some(widget) = child {
                if let Some(scroll) = find_scroll(&widget) { return Some(scroll); }
                child = widget.next_sibling();
            }
            None
        }
        fn log_visible_terminals(widget: &gtk::Widget, visible: bool) {
            if let Ok(terminal) = widget.clone().downcast::<vte4::Terminal>() {
                if terminal.is_mapped() {
                    let adjustment = gtk::prelude::ScrollableExt::vadjustment(&terminal);
                    log::debug!(target: "anvil::workspace_shell", "tools_native_vte visible={} identity={} adjustment={:?} cursor={:?} columns={} rows={}", visible, terminal.as_ptr() as usize, adjustment.map(|a| a.value()), terminal.cursor_position(), terminal.column_count(), terminal.row_count());
                }
                return;
            }
            let mut child = widget.first_child();
            while let Some(widget) = child {
                log_visible_terminals(&widget, visible);
                child = widget.next_sibling();
            }
        }
        log_visible_terminals(&current, popover.is_visible());
        if let Some(scroll) = find_scroll(&current) {
            let adjustment = scroll.vadjustment();
            let anchor = scroll.child().and_then(|list| {
                let mut child = list.first_child();
                while let Some(widget) = child {
                    if let Some(bounds) = widget.compute_bounds(&scroll) {
                        if bounds.y() + bounds.height() > 0.0 && bounds.y() < adjustment.page_size() as f32 {
                            return Some((widget.as_ptr() as usize, bounds.y()));
                        }
                    }
                    child = widget.next_sibling();
                }
                None
            });
            log::debug!(target: "anvil::workspace_shell", "tools_popup visible={} adjustment={} upper={} page={} native_anchor={anchor:?}", popover.is_visible(), adjustment.value(), adjustment.upper(), adjustment.page_size());
        }
    });
    row.append(&tools);
    let hint = label("Shell · Tab completes", "chat-input-hint");
    hint.set_visible(false);
    row.append(&hint);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    row.append(&spacer);
    let model = unavailable(tokens::MODEL_UNAVAILABLE_LABEL);
    model.add_css_class("chat-tool-capsule");
    model.set_width_request(tokens::COMPOSER_MODEL_MIN_WIDTH as i32);
    model.set_tooltip_text(Some("No agent model is connected to this shell input"));
    row.append(&model);
    send.add_css_class("chat-send");
    send.set_child(Some(&footer_icon(Icon::Send)));
    row.append(send);
    if let Some(stop) = stop {
        row.append(stop);
    }
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&row));
    let observer = gtk::DrawingArea::new();
    observer.set_can_target(false);
    observer.set_focusable(false);
    overlay.add_overlay(&observer);
    overlay.set_measure_overlay(&observer, false);
    let weak_secondary = secondary_scroll.downgrade();
    let weak_tools_grid = tools_grid.downgrade();
    observer.connect_resize(move |_, width, _| {
        if let Some(secondary) = weak_secondary.upgrade() {
            // Model and Send keep their slots. The same live Terminal focus
            // action remains available in Tools when secondary content yields.
            secondary.set_visible(width >= 310);
        }
        if let Some(grid) = weak_tools_grid.upgrade() {
            // Decorative grid yields before the 120px model or Send slot.
            // Tools text, chevron and every menu action remain reachable.
            grid.set_visible(width >= 310);
        }
    });
    let controls = gtk::Box::new(gtk::Orientation::Vertical, 0);
    controls.add_css_class("chat-controls");
    own_composer_chrome(&controls);
    controls.set_height_request(tokens::CONTROL_HEIGHT as i32);
    controls.set_vexpand(true);
    controls.set_valign(gtk::Align::End);
    controls.append(&overlay);
    (controls, hint)
}

impl Chrome {
    pub(crate) fn new(
        sender: &ComponentSender<AppModel>,
        terminal_stack: &gtk::Stack,
        navigation: &gtk::Box,
        side_stack: &gtk::Stack,
    ) -> Self {
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        top.add_css_class("workspace-titlebar");
        top.set_height_request(tokens::TITLEBAR_HEIGHT as i32);
        let identity = gtk::Box::new(gtk::Orientation::Horizontal, 5);
        identity.set_width_request((tokens::RAIL_WIDTH + tokens::NAVIGATION_WIDTH - 12.0) as i32);
        for (icon, title, msg) in [
            ("window-close-symbolic", "Close window", AppMsg::Quit),
            (
                "window-minimize-symbolic",
                "Minimize window",
                AppMsg::MinimizeWindow,
            ),
            (
                "window-maximize-symbolic",
                "Maximize or restore window",
                AppMsg::ToggleMaximizedWindow,
            ),
        ] {
            let control = button(icon, title, sender, msg);
            control.add_css_class("shell-window-control");
            control.set_size_request(24, 24);
            control.set_valign(gtk::Align::Center);
            identity.append(&control);
        }
        let app_icon = product_icon(
            "anvil",
            jterm_core::reference_identity::PRODUCT_ICON_SIZE as i32,
        );
        app_icon.add_css_class("shell-brand-icon");
        identity.append(&app_icon);
        identity.append(&label("anvil", "shell-brand"));

        top.append(&identity);
        let path = label("Terminal workspace", "shell-top-path");
        path.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        path.set_width_chars(12);
        path.set_max_width_chars(22);
        top.append(&path);
        let search = gtk::Button::new();
        search.add_css_class("shell-search");
        search.set_hexpand(true);
        search.set_focus_on_click(false);
        search.set_tooltip_text(Some("Open command center (Ctrl+Shift+P)"));
        let search_content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        search_content.append(&icon(jterm_core::reference_icons::Icon::Search));
        let search_text = label("Search commands, files, sessions…", "shell-muted");
        search_text.set_hexpand(true);
        search_text.set_ellipsize(gtk::pango::EllipsizeMode::End);
        search_content.append(&search_text);
        search_content.append(&label("Ctrl ⇧ P", "shell-keycap"));
        search.set_child(Some(&search_content));
        let search_sender = sender.clone();
        search.connect_clicked(move |_| {
            search_sender.input(AppMsg::Action(Action::ToggleCommandPalette))
        });
        top.append(&search);
        let modes = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        modes.add_css_class("shell-modes");
        modes.set_valign(gtk::Align::Center);
        modes.set_height_request(28);
        let mut mode_buttons = Vec::new();
        for (title, index) in [("Block", 0), ("Chat", 1)] {
            let mode_button = gtk::Button::with_label(title);
            mode_button.add_css_class("flat");
            mode_button.set_focus_on_click(false);
            mode_button.set_tooltip_text(Some(
                "Choose the presentation for new panes; existing shells remain running",
            ));
            let mode_sender = sender.clone();
            mode_button
                .connect_clicked(move |_| mode_sender.input(AppMsg::SettingsTerminalMode(index)));
            modes.append(&mode_button);
            mode_buttons.push(mode_button);
        }
        top.append(&modes);
        top.append(&button(
            "view-grid-symbolic",
            "Settings",
            sender,
            AppMsg::Action(Action::ToggleSettings),
        ));

        // The launcher is centered independently of the left/right controls;
        // extra window width belongs to the canvas, not an expanding search.
        top.remove(&search);
        let title_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        title_spacer.set_hexpand(true);
        top.insert_child_after(&title_spacer, Some(&path));
        search.set_hexpand(false);
        search.set_width_request(tokens::SEARCH_WIDTH as i32);
        search.set_height_request((tokens::SEARCH_HEIGHT - 2.0) as i32);
        search.set_halign(gtk::Align::Center);
        search.set_valign(gtk::Align::Start);
        search.set_margin_start((tokens::SEARCH_CENTER_OFFSET * 2.0) as i32);
        search.set_margin_top(tokens::SEARCH_TOP as i32);
        let title_overlay = gtk::Overlay::new();
        title_overlay.set_child(Some(&top));
        title_overlay.add_overlay(&search);
        title_overlay.set_measure_overlay(&search, false);
        let title_observer = gtk::DrawingArea::new();
        title_observer.set_can_target(false);
        title_overlay.add_overlay(&title_observer);
        title_overlay.set_measure_overlay(&title_observer, false);
        let search_width_gate = search.downgrade();
        let path_width_gate = path.downgrade();
        title_observer.connect_resize(move |_, width, _| {
            if let Some(search) = search_width_gate.upgrade() {
                search.set_visible(width >= 1024);
            }
            if let Some(path) = path_width_gate.upgrade() {
                path.set_visible(width >= 1120);
            }
        });
        let top = title_overlay;

        let rail = gtk::Box::new(gtk::Orientation::Vertical, 12);
        rail.add_css_class("workspace-rail");
        rail.set_width_request(tokens::RAIL_WIDTH as i32);
        rail.set_hexpand(false);
        rail.set_vexpand(true);
        for (key, title) in jterm_core::reference_conversation::RAIL_NAVIGATION {
            let action = match key {
                "workspace" => AppMsg::SetSidebarView(crate::config::SidebarView::Tabs),
                "folder" => AppMsg::SetSidebarView(crate::config::SidebarView::Files),
                "terminal" => AppMsg::FocusShell,
                "agent" => AppMsg::Action(Action::ToggleTasksPanel),
                _ => AppMsg::Action(Action::ToggleSearch),
            };
            let control = button(key, title, sender, action);
            control.set_size_request(
                tokens::SELECTED_RAIL_SIZE as i32,
                tokens::SELECTED_RAIL_SIZE as i32,
            );
            control.set_halign(gtk::Align::Center);
            rail.append(&control);
        }
        if let Some(selected) = rail.first_child() {
            selected.add_css_class("shell-selected");
        }
        let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
        spacer.set_vexpand(true);
        rail.append(&spacer);
        rail.append(&button(
            "preferences-system-symbolic",
            "Settings",
            sender,
            AppMsg::Action(Action::ToggleSettings),
        ));
        let initial = std::env::var("USER")
            .ok()
            .and_then(|user| user.chars().next())
            .unwrap_or('U')
            .to_uppercase()
            .to_string();
        let profile = label(&initial, "shell-profile");
        profile.set_size_request(
            tokens::PROFILE_DIAMETER as i32,
            tokens::PROFILE_DIAMETER as i32,
        );
        profile.set_halign(gtk::Align::Center);
        profile.set_xalign(0.5);
        rail.append(&profile);

        navigation.add_css_class("workspace-navigation");
        navigation.set_width_request(
            (tokens::NAVIGATION_WIDTH - 2.0 * tokens::NAVIGATION_PADDING) as i32,
        );
        let navigation_heading = gtk::Box::new(
            gtk::Orientation::Vertical,
            tokens::NAVIGATION_ROW_GAP as i32,
        );
        navigation_heading.append(&label("WORKSPACE", "shell-section"));
        let workspace = action_row(
            "folder-symbolic",
            "Local workspace",
            sender,
            AppMsg::Action(Action::ToggleCommandPalette),
        );
        workspace.add_css_class("shell-workspace-selector");
        let selector = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        selector.append(&icon(jterm_core::reference_icons::Icon::Files));
        let workspace_name = label("Local workspace", "shell-navigation-title");
        workspace_name.set_hexpand(true);
        workspace_name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        selector.append(&workspace_name);
        let branch = label("—", "shell-workspace-branch");
        branch.set_tooltip_text(Some("Branch information is unavailable"));
        selector.append(&branch);
        selector.append(&icon(jterm_core::reference_icons::Icon::ChevronDown));
        workspace.set_child(Some(&selector));
        workspace.set_margin_top(1);
        navigation_heading.append(&workspace);
        let mut presentation_navigation = None;
        for (index, (key, title)) in jterm_core::reference_conversation::NAVIGATION
            .into_iter()
            .enumerate()
        {
            let (glyph, action) = match key {
                "conversation" => (
                    "conversation",
                    AppMsg::SetSidebarView(crate::config::SidebarView::Tabs),
                ),
                "folder" => (
                    "folder-open-symbolic",
                    AppMsg::SetSidebarView(crate::config::SidebarView::Files),
                ),
                "terminal" => ("utilities-terminal-symbolic", AppMsg::FocusShell),
                "agent" => (
                    "system-run-symbolic",
                    AppMsg::Action(Action::ToggleTasksPanel),
                ),
                _ => (
                    "preferences-system-symbolic",
                    AppMsg::Action(Action::ToggleSettings),
                ),
            };
            let row = action_row(glyph, title, sender, action);
            if index == 0 {
                row.set_margin_top(5);
                row.add_css_class("shell-selected");
                presentation_navigation = row
                    .child()
                    .and_downcast::<gtk::Box>()
                    .and_then(|row| row.last_child())
                    .and_downcast::<gtk::Label>();
            }
            navigation_heading.append(&row);
        }
        let recent_heading = label("RECENT", "shell-section");
        recent_heading.add_css_class("shell-recent-heading");
        navigation_heading.append(&recent_heading);
        navigation.prepend(&navigation_heading);
        let spaces = gtk::Box::new(gtk::Orientation::Vertical, 0);
        spaces.add_css_class("shell-spaces");
        spaces.append(&label("SPACES", "shell-section"));
        for (index, (name, description)) in jterm_core::reference_conversation::SPACES
            .into_iter()
            .enumerate()
        {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.set_height_request(tokens::SPACES_ROW_HEIGHT as i32);
            let identity = gtk::CenterBox::new();
            identity.set_size_request(22, 22);
            identity.set_hexpand(false);
            identity.set_halign(gtk::Align::Start);
            identity.set_valign(gtk::Align::Center);
            identity.add_css_class("shell-space-icon");
            identity.add_css_class(&format!("shell-space-brand-{index}"));
            let glyph = product_icon(name, jterm_core::reference_identity::SPACE_ICON_SIZE as i32);
            identity.set_center_widget(Some(&glyph));
            row.append(&identity);
            let text = gtk::Box::new(gtk::Orientation::Vertical, 1);
            let name_line = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            name_line.append(&label(name, "shell-navigation-title"));
            name_line.append(&label("Unavailable", "shell-muted"));
            text.append(&name_line);
            text.append(&label(description, "shell-muted"));
            row.append(&text);
            spaces.append(&row);
        }
        let status = gtk::Box::new(gtk::Orientation::Vertical, 6);
        status.add_css_class("workspace-navigation-footer");
        status.set_height_request(tokens::NAVIGATION_STATUS_HEIGHT as i32);
        status.set_tooltip_text(Some("Local terminal session"));
        let status_line = gtk::Box::new(gtk::Orientation::Horizontal, 5);
        let dot = gtk::DrawingArea::new();
        dot.set_content_width(5);
        dot.set_content_height(5);
        dot.set_valign(gtk::Align::Center);
        dot.set_draw_func(|_, cr, _, _| {
            cr.set_source_rgb(0.51, 0.58, 0.69);
            cr.arc(2.5, 2.5, 2.0, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
        });
        status_line.append(&dot);
        status_line.append(&label("Local session", "shell-muted"));
        status.append(&status_line);
        status.append(&label("Local workspace", "shell-muted"));
        navigation.append(&spaces);

        let center = gtk::Box::new(gtk::Orientation::Vertical, 0);
        center.add_css_class("workspace-center");
        center.set_hexpand(true);
        center.set_vexpand(true);
        let header = gtk::Box::new(
            gtk::Orientation::Horizontal,
            tokens::HEADER_ACTION_GAP as i32,
        );
        header.add_css_class("workspace-canvas-header");
        header.add_css_class("anvil-ui-font");
        header.set_height_request(tokens::CANVAS_HEADER_HEIGHT as i32);
        let headings = gtk::Box::new(gtk::Orientation::Vertical, 3);
        headings.set_hexpand(true);
        let title = label("Terminal workspace", "shell-canvas-title");
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let mode = label("Block · Live local shell", "shell-canvas-subtitle");
        headings.append(&title);
        headings.append(&mode);
        header.append(&headings);
        header.append(&button(
            "list-add-symbolic",
            "New session",
            sender,
            AppMsg::NewTab,
        ));
        header.append(&button(
            "system-search-symbolic",
            "Search terminal output",
            sender,
            AppMsg::Action(Action::ToggleSearch),
        ));
        header.append(&button(
            "view-more-symbolic",
            "Command center",
            sender,
            AppMsg::Action(Action::ToggleCommandPalette),
        ));
        center.append(&header);
        center.append(terminal_stack);
        terminal_stack.set_vexpand(true);
        // Block keeps the existing terminal as its sole input owner. Its
        // matching composer frame exposes shared tools and an honest hint;
        // it is never a second text editor or a second submission path.
        let block_shelf = gtk::Box::new(gtk::Orientation::Vertical, 4);
        block_shelf.add_css_class("shell-block-composer");
        block_shelf.set_vexpand(false);
        block_shelf.set_valign(gtk::Align::End);
        block_shelf.set_height_request(tokens::COMPOSER_HEIGHT as i32);
        block_shelf.append(&label(
            "Type in the terminal above",
            "shell-block-input-hint",
        ));
        let send = unavailable("Send");
        send.set_tooltip_text(Some("Use Enter in the active terminal to submit"));
        let focus_sender = sender.clone();
        let (block_tools, _) = composer_tools(
            &send,
            None,
            std::rc::Rc::new(move || focus_sender.input(AppMsg::FocusShell)),
            {
                let sender = sender.clone();
                std::rc::Rc::new(move |action| sender.input(AppMsg::Action(action)))
            },
        );
        block_shelf.append(&block_tools);
        center.append(&block_shelf);

        let inspector = gtk::Box::new(gtk::Orientation::Vertical, 0);
        inspector.add_css_class("workspace-inspector");
        inspector.add_css_class("anvil-ui-font");
        inspector.set_width_request((tokens::INSPECTOR_WIDTH - 1.0) as i32);
        let inspector_tabs = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        inspector_tabs.add_css_class("shell-inspector-tabs");
        inspector_tabs.set_height_request(tokens::CANVAS_HEADER_HEIGHT as i32);
        let panel = gtk::Stack::new();
        panel.set_hhomogeneous(false);
        panel.set_vhomogeneous(false);
        panel.set_vexpand(true);
        let fixture = jterm_core::reference_conversation::fixture_enabled();
        let inspector_model = if fixture {
            jterm_core::reference_conversation::acceptance_fixture().inspector
        } else {
            jterm_core::reference_conversation::Inspector::empty()
        };
        let overview = crate::workspace_conversation::inspector(&inspector_model, fixture);
        let overview_scroll = gtk::ScrolledWindow::new();
        overview_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        overview_scroll.set_child(Some(&overview));
        panel.add_named(&overview_scroll, Some("overview"));
        panel.add_named(side_stack, Some("live"));
        let context_page = gtk::Box::new(gtk::Orientation::Vertical, 12);
        context_page.add_css_class("shell-agent-card");
        context_page.append(&label("SESSION CONTEXT", "shell-section"));
        let context_details = label("Local terminal", "shell-inspector-path");
        context_details.set_wrap(true);
        context_details.set_selectable(true);
        context_page.append(&context_details);
        let context_notice = label("No terminal content is sent automatically. Agent context sharing follows your consent settings.", "shell-muted");
        context_notice.set_wrap(true);
        context_page.append(&context_notice);
        context_page.set_valign(gtk::Align::Start);
        panel.add_named(&context_page, Some("context"));
        panel.set_visible_child_name("overview");
        let live_panel = panel.downgrade();
        side_stack.connect_visible_notify(move |stack| {
            if let Some(panel) = live_panel.upgrade() {
                panel.set_visible_child_name(if stack.is_visible() {
                    "live"
                } else {
                    "overview"
                });
            }
        });
        let agent_tab = gtk::Button::with_label("Agent");
        let context_tab = gtk::Button::with_label("Context");
        for (tab, page) in [(&agent_tab, "overview"), (&context_tab, "context")] {
            tab.add_css_class("flat");
            tab.set_width_request(tokens::INSPECTOR_TAB_WIDTH as i32);
            tab.set_valign(gtk::Align::Center);
            tab.set_height_request(28);
            if page == "overview" {
                tab.add_css_class("shell-selected");
            }
            let panel = panel.clone();
            let agent = agent_tab.clone();
            let context = context_tab.clone();
            tab.connect_clicked(move |_| {
                panel.set_visible_child_name(page);
                agent.remove_css_class("shell-selected");
                context.remove_css_class("shell-selected");
                if page == "overview" {
                    agent.add_css_class("shell-selected");
                } else {
                    context.add_css_class("shell-selected");
                }
            });
            inspector_tabs.append(tab);
        }
        let files_tab = unavailable("Files");
        files_tab.set_tooltip_text(Some("Right-side file inspection is not connected yet. Project files are available in the left navigation."));
        files_tab.set_width_request(tokens::INSPECTOR_TAB_WIDTH as i32);
        files_tab.set_valign(gtk::Align::Center);
        files_tab.set_height_request(28);
        inspector_tabs.append(&files_tab);
        let tab_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        tab_spacer.set_hexpand(true);
        inspector_tabs.append(&tab_spacer);
        let inspector_options = unavailable("Options");
        inspector_options.set_child(Some(&icon(jterm_core::reference_icons::Icon::Grid)));
        inspector_options.set_size_request(24, 24);
        inspector_options.set_valign(gtk::Align::Center);
        inspector_tabs.append(&inspector_options);
        inspector.append(&inspector_tabs);
        inspector.append(&panel);
        Self {
            top,
            rail,
            center,
            canvas_header: header,
            block_composer_chrome: block_tools,
            inspector,
            status,
            title,
            path,
            workspace_name,
            mode,
            block_mode: mode_buttons.remove(0),
            chat_mode: mode_buttons.remove(0),
            context_details,
            block_shelf,
            presentation_navigation: presentation_navigation
                .expect("presentation navigation label"),
            search,
        }
    }

    pub(crate) fn sync(&self, title: &str, path: &str, mode: TerminalMode, private: bool) {
        if log::log_enabled!(target: "anvil::workspace_shell", log::Level::Debug) {
            if let Some(bounds) = self.search.compute_bounds(&self.top) {
                log::debug!(target: "anvil::workspace_shell", "launcher_geometry x={} y={} width={} height={}", bounds.x(), bounds.y(), bounds.width(), bounds.height());
            }
        }
        let path = if private { "Private session" } else { path };
        self.title.set_label(title);
        self.title.set_tooltip_text(Some(title));
        self.path.set_label(path);
        self.path.set_tooltip_text(Some(path));
        self.workspace_name.set_label(if private {
            "Private session"
        } else {
            std::path::Path::new(path)
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("Local workspace")
        });
        self.workspace_name.set_tooltip_text(Some(path));
        self.context_details.set_label(path);
        self.status.set_tooltip_text(Some(path));
        if mode == TerminalMode::Block {
            self.center.add_css_class("block-workspace");
        } else {
            self.center.remove_css_class("block-workspace");
        }
        // Block's unverified dock is not active in this checkpoint.
        self.block_shelf.set_visible(mode != TerminalMode::Chat);
        self.canvas_header
            .set_height_request(if mode == TerminalMode::Block {
                42
            } else {
                tokens::CANVAS_HEADER_HEIGHT as i32
            });
        if mode != TerminalMode::Chat
            && log::log_enabled!(target: "anvil::workspace_shell", log::Level::Debug)
        {
            if let Some(window) = self.block_shelf.root().and_downcast::<gtk::Window>() {
                if let Some(bounds) = self.block_shelf.compute_bounds(&window) {
                    log::debug!(target: "anvil::workspace_shell", "block_composer_geometry x={} y={} width={} height={}",bounds.x(),bounds.y(),bounds.width(),bounds.height());
                }
            }
        }
        self.block_mode.remove_css_class("shell-current-mode");
        self.chat_mode.remove_css_class("shell-current-mode");
        if mode == TerminalMode::Chat {
            self.chat_mode.add_css_class("shell-current-mode");
        } else {
            self.block_mode.add_css_class("shell-current-mode");
        }
        let mode = if mode == TerminalMode::Chat {
            "Chat"
        } else {
            "Block"
        };
        self.presentation_navigation.set_label(mode);
        if jterm_core::reference_conversation::fixture_enabled() {
            let fixture = jterm_core::reference_conversation::acceptance_fixture();
            self.title.set_label(&fixture.title);
            self.mode
                .set_label(jterm_core::reference_conversation::FIXTURE_LABEL);
        } else {
            self.mode.set_label(&format!("{mode} · Local session"));
        }
    }
}

/// Paint-only contract. Native terminal and canvas geometry remain backend-owned.
pub(crate) fn install_css() {
    let css = include_str!("workspace_shell.css");
    let provider = gtk::CssProvider::new();
    provider.load_from_string(css);
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 20,
        );
    }
}

thread_local! {
    static UI_FONT_PROVIDER: std::cell::RefCell<Option<gtk::CssProvider>> = const {
        std::cell::RefCell::new(None)
    };
    static CONTENT_FONT_PROVIDER: std::cell::RefCell<Option<gtk::CssProvider>> = const {
        std::cell::RefCell::new(None)
    };
}

/// Keep the reference roles' different base sizes within their owning subtree.
/// The original stylesheet remains the source of those sizes; a
/// display-wide generic label override would flatten the role hierarchy.
fn role_font_css(scale: f64, scope: &str) -> String {
    static RULES: std::sync::OnceLock<Vec<(Vec<String>, f64)>> = std::sync::OnceLock::new();
    let rules = RULES.get_or_init(|| {
        let comments = regex::Regex::new(r"(?s)/\*.*?\*/").unwrap();
        let source = comments.replace_all(include_str!("workspace_shell.css"), "");
        let blocks = regex::Regex::new(r"(?s)([^{}]+)\{([^{}]*)\}").unwrap();
        let font_size =
            regex::Regex::new(r"(?:^|;)\s*font-size\s*:\s*([0-9.]+)px\s*(?:;|$)").unwrap();
        blocks
            .captures_iter(&source)
            .filter_map(|rule| {
                let size = font_size.captures(&rule[2])?[1].parse::<f64>().ok()?;
                let selectors = rule[1]
                    .split(',')
                    .map(str::trim)
                    .filter(|selector| !selector.is_empty())
                    .map(str::to_owned)
                    .collect();
                Some((selectors, size))
            })
            .collect()
    });
    // UI scale and size are validated independently. Their combined ratio
    // can legitimately exceed the scale-only range (for example 28pt * 10).
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let mut css = String::new();
    for (selectors, size) in rules {
        let mut scoped = Vec::new();
        for selector in selectors {
            scoped.push(format!("window.anvil-font-sync {scope} {selector}"));
            // A capture root can itself carry this role (rather than being
            // an ancestor). Add the same specificity in either arrangement.
            for root in [
                ".workspace-titlebar",
                ".workspace-navigation",
                ".workspace-inspector",
                ".workspace-canvas-header",
                ".reference-composer-chrome",
                ".block-reference-header",
                ".block-reference-toolbar",
            ] {
                if selector.strip_prefix(root).is_some_and(|suffix| {
                    suffix
                        .chars()
                        .next()
                        .is_none_or(|next| matches!(next, ' ' | '.' | ':' | '[' | '>' | '+' | '~'))
                }) {
                    let owned = selector.replacen(root, &format!("{root}{scope}"), 1);
                    scoped.push(format!("window.anvil-font-sync {owned}"));
                    break;
                }
            }
        }
        css.push_str(&format!(
            "{} {{ font-size: {}px; }}\n",
            scoped.join(","),
            size * scale
        ));
    }
    css
}

pub(crate) fn apply_ui_font_scale(scale: f64) {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    UI_FONT_PROVIDER.with(|slot| {
        let mut slot = slot.borrow_mut();
        let provider = slot.get_or_insert_with(|| {
            let provider = gtk::CssProvider::new();
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 21,
            );
            provider
        });
        provider.load_from_string(&role_font_css(scale, ".anvil-ui-font"));
    });
}

/// Authored message roles belong to the document, not the surrounding chrome
/// or composer. Their owner is the existing transcript subtree; geometry and
/// reading/follow intent remain with that retained canvas.
pub(crate) fn apply_content_font_scale(scale: f64) {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    CONTENT_FONT_PROVIDER.with(|slot| {
        let mut slot = slot.borrow_mut();
        let provider = slot.get_or_insert_with(|| {
            let provider = gtk::CssProvider::new();
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 21,
            );
            provider
        });
        provider.load_from_string(&role_font_css(scale, ".anvil-content-font"));
    });
}

pub(crate) fn apply_canvas_palette(config: &mut Config) {
    // This is called only on the pane's private configuration snapshot.
    // A custom palette or explicit theme remains under the user's control.
    if !matches!(
        config.terminal_mode,
        TerminalMode::Chat | TerminalMode::Block
    ) || config.theme_name != "default"
    {
        return;
    }
    if let Some(default) = crate::config::builtin_themes()
        .into_iter()
        .find(|theme| theme.name == "default")
    {
        if config.foreground != default.foreground
            || config.background != default.background
            || config.palette != default.palette
        {
            return;
        }
    }
    let rgba = |rgb: [u8; 3]| {
        gtk::gdk::RGBA::new(
            rgb[0] as f32 / 255.0,
            rgb[1] as f32 / 255.0,
            rgb[2] as f32 / 255.0,
            1.0,
        )
    };
    if config.terminal_mode == TerminalMode::Block {
        // Private pane presentation only; ANSI palette, configured fonts and
        // the persisted/user configuration keep their original authority.
        config.foreground = rgba([0xD5, 0xDE, 0xEC]);
        config.background = rgba([0x0E, 0x16, 0x21]);
        return;
    }
    if let Some(light) = crate::config::builtin_themes()
        .into_iter()
        .find(|theme| theme.name == "light")
    {
        config.palette = light.palette;
    }
    config.foreground = rgba(tokens::color::CANVAS_TEXT);
    config.background = rgba(tokens::color::CANVAS);
    config.cursor = rgba(tokens::color::ACCENT);
    config.cursor_foreground = rgba(tokens::color::COMPOSER);
}

pub(crate) fn recent_fixture() -> gtk::Box {
    let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
    list.add_css_class("shell-recent-list");
    list.set_height_request(180);
    for entry in jterm_core::reference_conversation::fixture_recent_entries() {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let body = gtk::Box::new(gtk::Orientation::Vertical, 2);
        body.set_hexpand(true);
        let title = label(&entry.title, "shell-navigation-title");
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        title.set_width_chars(1);
        title.set_tooltip_text(Some(&entry.title));
        body.append(&title);
        let detail = label(&entry.detail, "shell-muted");
        detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
        detail.set_width_chars(1);
        body.append(&detail);
        let history = icon(jterm_core::reference_icons::Icon::from_key(entry.icon_key));
        history.set_content_width(12);
        history.set_content_height(12);
        row.append(&history);
        row.append(&body);
        row.append(&label(&entry.time, "shell-muted"));
        row.set_tooltip_text(Some("Visual acceptance fixture · sample recent entry"));
        row.add_css_class("shell-recent-row");
        row.set_height_request(tokens::RECENT_ROW_HEIGHT as i32);
        if entry.selected {
            row.add_css_class("shell-selected");
        }
        list.append(&row);
    }
    list
}

/// GTK allocation signals observe size changes without a permanent frame tick.
pub(crate) fn observe_layout(
    host: &gtk::Overlay,
    navigation: &gtk::Widget,
    inspector: &gtk::Box,
    navigation_split: &gtk::Paned,
    inspector_split: &gtk::Paned,
) {
    let observer = gtk::DrawingArea::new();
    observer.set_can_target(false);
    observer.set_focusable(false);
    observer.set_halign(gtk::Align::Fill);
    observer.set_valign(gtk::Align::Fill);
    host.add_overlay(&observer);
    host.set_measure_overlay(&observer, false);
    let navigation = navigation.downgrade();
    let inspector = inspector.downgrade();
    let navigation_split = navigation_split.downgrade();
    let inspector_split = inspector_split.downgrade();
    observer.connect_resize(move |_, width, height| {
        let layout =
            tokens::Layout::for_size(width as f32, height as f32 + tokens::TITLEBAR_HEIGHT);
        if let Some(navigation) = navigation.upgrade() {
            navigation.set_visible(layout.navigation.is_some());
        }
        if let Some(inspector) = inspector.upgrade() {
            inspector.set_visible(layout.inspector.is_some());
        }
        if let Some(split) = navigation_split.upgrade() {
            split.set_position(if layout.navigation.is_some() {
                tokens::NAVIGATION_WIDTH as i32
            } else {
                0
            });
        }
        if let Some(split) = inspector_split.upgrade() {
            let inner = width as f32
                - tokens::RAIL_WIDTH
                - if layout.navigation.is_some() {
                    tokens::NAVIGATION_WIDTH
                } else {
                    0.0
                };
            split.set_position(
                (inner
                    - if layout.inspector.is_some() {
                        tokens::INSPECTOR_WIDTH
                    } else {
                        0.0
                    }) as i32,
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a native GTK display"]
    fn reference_ui_size_preserves_nominal_density_and_point_pixel_units() {
        use gtk::pango::prelude::FontExt;

        gtk::init().unwrap();
        install_css();
        let window = gtk::Window::new();
        window.add_css_class("anvil-font-sync");
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let header = gtk::Box::new(gtk::Orientation::Vertical, 0);
        header.add_css_class("workspace-canvas-header");
        header.add_css_class("anvil-ui-font");
        let title = label("Canvas header", "shell-canvas-title");
        header.append(&title);
        column.append(&header);
        let document = gtk::Box::new(gtk::Orientation::Vertical, 0);
        document.add_css_class("native-conversation");
        let content = label("Authored content", "conversation-heading");
        document.append(&content);
        column.append(&document);
        window.set_child(Some(&column));
        window.present();
        let context = window.pango_context();
        let nominal = gtk::pango::FontDescription::from_string("sans-serif 14");
        let nominal_px = f64::from(
            context
                .load_font(&nominal)
                .unwrap()
                .describe_with_absolute_size()
                .size(),
        ) / f64::from(gtk::pango::SCALE);
        let mut config = Config::safe_defaults();
        assert_eq!(
            crate::font::reference_ui_role_multiplier(&config, &context),
            1.0
        );
        // A frozen legacy point or absolute-pixel UI value must not jump when
        // Content is subsequently edited. This uses the unchanged migration.
        for legacy in ["Monospace 14", "Monospace 16px"] {
            config.font_desc = legacy.into();
            config.ui_font_size = None;
            config.ui_font_scale = None;
            config.default_font_scale = 1.5;
            let before = crate::font::reference_ui_role_multiplier(&config, &context);
            crate::font::freeze_ui_font(&mut config);
            config.default_font_scale = 1.8;
            config.font_desc = "Monospace 32".into();
            assert_eq!(
                crate::font::reference_ui_role_multiplier(&config, &context),
                before
            );
        }
        for (size, absolute, scale) in [
            (14.0, false, 1.0),
            (28.0, false, 1.0),
            (14.0, true, 1.0),
            (28.0, true, 1.0),
            (28.0, false, 1.3),
            (28.0, true, 1.3),
            (72.0, false, 10.0),
        ] {
            config.ui_font_size = Some(size);
            config.ui_font_size_absolute = absolute;
            config.ui_font_scale = Some(scale);
            let factor = crate::font::reference_ui_role_multiplier(&config, &context);
            let expected = if absolute {
                size / nominal_px
            } else {
                size / 14.0
            } * scale;
            assert!((factor - expected).abs() < 0.001, "{factor} vs {expected}");
            apply_ui_font_scale(factor);
            let main = gtk::glib::MainContext::default();
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
            while std::time::Instant::now() < deadline {
                while main.iteration(false) {}
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            let raw = title.pango_context().font_description().unwrap().size();
            let expected_raw = (12.0 * factor * f64::from(gtk::pango::SCALE)).round() as i32;
            assert!((raw - expected_raw).abs() <= 1, "{raw} vs {expected_raw}");
            assert_eq!(
                content.pango_context().font_description().unwrap().size(),
                20 * gtk::pango::SCALE
            );
            eprintln!(
                "ui-size: size={size} absolute={absolute} scale={scale} nominal_px={nominal_px} factor={factor} raw_header={raw} widget_scale={}",
                window.scale_factor()
            );
        }
        apply_ui_font_scale(1.0);
        window.close();
    }

    #[test]
    #[ignore = "requires a native GTK display"]
    fn reference_content_size_preserves_units_roles_and_ui_independence() {
        use gtk::pango::prelude::FontExt;
        gtk::init().unwrap();
        install_css();
        let window = gtk::Window::new();
        window.add_css_class("anvil-font-sync");
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let header = gtk::Box::new(gtk::Orientation::Vertical, 0);
        header.add_css_class("workspace-canvas-header");
        header.add_css_class("anvil-ui-font");
        let title = label("Canvas header", "shell-canvas-title");
        header.append(&title);
        column.append(&header);
        let transcript = gtk::Box::new(gtk::Orientation::Vertical, 0);
        transcript.add_css_class("anvil-content-font");
        let document = crate::workspace_conversation::render(
            &jterm_core::reference_conversation::acceptance_fixture(),
        );
        transcript.append(&document);
        column.append(&transcript);
        window.set_child(Some(&column));
        window.present();
        fn find(widget: &gtk::Widget, class: &str, within: bool) -> Option<gtk::Label> {
            let within = within || widget.has_css_class(class);
            if within {
                if let Ok(label) = widget.clone().downcast::<gtk::Label>() {
                    return Some(label);
                }
            }
            let mut child = widget.first_child();
            while let Some(node) = child {
                if let Some(label) = find(&node, class, within) {
                    return Some(label);
                }
                child = node.next_sibling();
            }
            None
        }
        let roles: Vec<_> = [
            ("conversation-heading", 20.0),
            ("conversation-paragraph", 12.0),
            ("conversation-chip", 9.0),
            ("conversation-step-title", 10.0),
            ("conversation-code-path", 10.0),
            ("conversation-code-text", 10.0),
            ("conversation-line-number", 9.0),
        ]
        .into_iter()
        .map(|(class, base)| {
            (
                find(document.upcast_ref(), class, false).expect(class),
                base,
            )
        })
        .collect();
        let context = window.pango_context();
        let mut config = Config::safe_defaults();
        config.ui_font_size = Some(14.0);
        config.ui_font_size_absolute = false;
        config.ui_font_scale = Some(1.0);
        let nominal = gtk::pango::FontDescription::from_string(&config.font_desc);
        assert_eq!(nominal.size(), 14 * gtk::pango::SCALE);
        assert!(
            !nominal.is_size_absolute(),
            "the production nominal remains 14pt"
        );
        let nominal_px = f64::from(
            context
                .load_font(&nominal)
                .unwrap()
                .describe_with_absolute_size()
                .size(),
        ) / f64::from(gtk::pango::SCALE);
        // Each point size is resolved to Pango units independently. Keep the
        // strict raw-unit assertion against native 28pt resolution instead of
        // assuming its quantized value is exactly twice resolved 14pt.
        let point_28 = gtk::pango::FontDescription::from_string("sans-serif 28");
        let point_28_px = f64::from(
            context
                .load_font(&point_28)
                .unwrap()
                .describe_with_absolute_size()
                .size(),
        ) / f64::from(gtk::pango::SCALE);
        let point_ratio = point_28_px / nominal_px;
        for (description, scale, expected_factor, ui) in [
            ("Monospace 14", 1.0, 1.0, 1.0),
            ("Monospace 28", 1.0, point_ratio, 1.0),
            ("Monospace 14px", 1.0, 14.0 / nominal_px, 1.0),
            ("Monospace 28px", 1.0, 28.0 / nominal_px, 1.0),
            ("Monospace 28", 1.3, point_ratio * 1.3, 1.0),
            ("Monospace 28px", 1.3, 36.4 / nominal_px, 1.0),
            ("Monospace 28", 10.0, point_ratio * 10.0, 1.0),
            ("Monospace 14", 1.0, 1.0, 1.3),
            ("Monospace 14", 1.0, 1.0, 1.0),
        ] {
            config.font_desc = description.into();
            config.default_font_scale = scale;
            config.ui_font_scale = Some(ui);
            apply_ui_font_scale(crate::font::reference_ui_role_multiplier(&config, &context));
            apply_content_font_scale(crate::font::reference_content_role_multiplier(
                &config, &context,
            ));
            let main = gtk::glib::MainContext::default();
            let until = std::time::Instant::now() + std::time::Duration::from_millis(100);
            while std::time::Instant::now() < until {
                while main.iteration(false) {}
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            for (label, base) in &roles {
                let actual = label.pango_context().font_description().unwrap().size();
                let expected =
                    (base * expected_factor * f64::from(gtk::pango::SCALE)).round() as i32;
                assert!(
                    (actual - expected).abs() <= 1,
                    "{description} scale={scale}: {} actual={actual} expected={expected}",
                    label.text()
                );
                let glyph = label.layout().pixel_size();
                assert!(
                    glyph.0 > 0 && glyph.1 > 0,
                    "a real role glyph must remain rendered"
                );
            }
            let actual = title.pango_context().font_description().unwrap().size();
            let expected = (12.0 * ui * f64::from(gtk::pango::SCALE)).round() as i32;
            assert!(
                (actual - expected).abs() <= 1,
                "Content size must not change Chrome"
            );
            eprintln!("content-size: desc={description} content_scale={scale} nominal_px={nominal_px} expected_factor={expected_factor} ui_scale={ui} dpr={} heading_raw={}", window.scale_factor(), roles[0].0.pango_context().font_description().unwrap().size());
        }
        apply_ui_font_scale(1.0);
        apply_content_font_scale(1.0);
        window.close();
    }

    #[test]
    #[ignore = "requires a native GTK display"]
    fn reference_content_roles_scale_independently_of_chrome() {
        gtk::init().unwrap();
        install_css();
        let window = gtk::Window::new();
        window.add_css_class("anvil-font-sync");
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let header = gtk::Box::new(gtk::Orientation::Vertical, 0);
        header.add_css_class("workspace-canvas-header");
        header.add_css_class("anvil-ui-font");
        let title = label("Canvas header", "shell-canvas-title");
        header.append(&title);
        column.append(&header);
        let transcript = gtk::Box::new(gtk::Orientation::Vertical, 0);
        transcript.add_css_class("anvil-content-font");
        let document = crate::workspace_conversation::render(
            &jterm_core::reference_conversation::acceptance_fixture(),
        );
        transcript.append(&document);
        column.append(&transcript);
        window.set_child(Some(&column));
        window.present();
        fn first_label(widget: &gtk::Widget) -> Option<gtk::Label> {
            if let Ok(label) = widget.clone().downcast::<gtk::Label>() {
                return Some(label);
            }
            let mut child = widget.first_child();
            while let Some(node) = child {
                if let Some(label) = first_label(&node) {
                    return Some(label);
                }
                child = node.next_sibling();
            }
            None
        }
        fn find(widget: &gtk::Widget, class: &str) -> Option<gtk::Label> {
            if widget.has_css_class(class) {
                if let Some(label) = first_label(widget) {
                    return Some(label);
                }
            }
            let mut child = widget.first_child();
            while let Some(node) = child {
                if let Some(label) = find(&node, class) {
                    return Some(label);
                }
                child = node.next_sibling();
            }
            None
        }
        let roles: Vec<_> = [
            ("conversation-heading", 20.0),
            ("conversation-paragraph", 12.0),
            ("conversation-chip", 9.0),
            ("conversation-step-title", 10.0),
            ("conversation-code-path", 10.0),
            ("conversation-code-text", 10.0),
            ("conversation-line-number", 9.0),
        ]
        .into_iter()
        .map(|(class, base)| {
            let widget = find(document.upcast_ref(), class).expect(class);
            (widget, base)
        })
        .collect();
        for (ui, content) in [(1.0, 1.0), (1.3, 1.0), (1.3, 1.3), (1.0, 1.3), (1.0, 1.0)] {
            apply_ui_font_scale(ui);
            apply_content_font_scale(content);
            let context = gtk::glib::MainContext::default();
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
            while std::time::Instant::now() < deadline {
                while context.iteration(false) {}
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            for (label, pixels) in roles
                .iter()
                .map(|(label, base)| (label, base * content))
                .chain(std::iter::once((&title, 12.0 * ui)))
            {
                let actual = label.pango_context().font_description().unwrap().size();
                let expected = (pixels * gtk::pango::SCALE as f64).round() as i32;
                assert!(
                    (actual - expected).abs() <= 1,
                    "{}: {actual} vs {expected}",
                    label.text()
                );
            }
        }
        window.close();
    }

    #[test]
    #[ignore = "requires a native GTK display"]
    fn reference_ui_font_roles_scale_without_flattening_content() {
        gtk::init().unwrap();
        install_css();
        let window = gtk::Window::new();
        window.add_css_class("anvil-font-sync");
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let header = gtk::Box::new(gtk::Orientation::Vertical, 0);
        header.add_css_class("workspace-canvas-header");
        header.add_css_class("anvil-ui-font");
        let title = label("Canvas header", "shell-canvas-title");
        header.append(&title);
        let inspector = gtk::Box::new(gtk::Orientation::Vertical, 0);
        inspector.add_css_class("workspace-inspector");
        inspector.add_css_class("anvil-ui-font");
        let task = label("Task title", "shell-agent-title");
        let metadata = label("Task metadata", "conversation-inspector-value");
        inspector.append(&task);
        inspector.append(&metadata);
        let document = gtk::Box::new(gtk::Orientation::Vertical, 0);
        document.add_css_class("native-conversation");
        let content = label("Content heading", "conversation-heading");
        document.append(&content);
        for part in [&header, &inspector, &document] {
            column.append(part);
        }
        window.set_child(Some(&column));
        window.present();
        for scale in [1.0, 1.3, 1.0] {
            apply_ui_font_scale(scale);
            let context = gtk::glib::MainContext::default();
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
            while std::time::Instant::now() < deadline {
                while context.iteration(false) {}
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            for (label, pixels) in [
                (&title, 12.0 * scale),
                (&task, 11.0 * scale),
                (&metadata, 9.0 * scale),
                (&content, 20.0),
            ] {
                let actual = label.pango_context().font_description().unwrap().size();
                let expected = (pixels * gtk::pango::SCALE as f64).round() as i32;
                assert!(
                    (actual - expected).abs() <= 1,
                    "{}: {actual} vs {expected}",
                    label.text()
                );
            }
        }
        window.close();
    }
}
