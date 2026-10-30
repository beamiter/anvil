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
use jterm_core::developer_workspace as tokens;
use relm4::{gtk, ComponentSender};
use vte4::TerminalExt;

const COMPOSER_CONTROL_HEIGHT: f32 = 22.0;

fn composer_control_height(scale: f64) -> i32 {
    f64::from(COMPOSER_CONTROL_HEIGHT)
        .max(14.0 * scale + 6.0)
        .ceil() as i32
}

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
    context_state: gtk::Label,
    context_copy: gtk::Button,
    context_value: std::rc::Rc<std::cell::RefCell<String>>,
    workspace_menu: gtk::Box,
    workspace_popover: gtk::Popover,
    workspace_entries: std::cell::RefCell<Vec<(u64, String, bool)>>,
    overview_host: gtk::Box,
    overview_model: std::cell::RefCell<Option<jterm_core::reference_conversation::Inspector>>,
    overview_actions: gtk::Box,
    overview_action_state: std::cell::RefCell<Vec<(String, crate::agent_task_ui::TaskPanelAction)>>,
    task_actions_available: std::cell::Cell<Option<(bool, bool)>>,
    sender: ComponentSender<AppModel>,
    files_control: std::cell::RefCell<Option<std::rc::Rc<FilesControl>>>,
    block_shelf: gtk::Box,
    presentation_navigation: gtk::Label,
    search: gtk::Button,
    inspector_control: std::rc::Rc<InspectorControl>,
    inspector_pages: gtk::Stack,
    sidebar_widths: std::rc::Rc<SidebarWidths>,
}

/// One authoritative browser moves between presentation hosts. Its model,
/// filesystem authority, pending intents and selection are never replaced.
struct FilesControl {
    home: gtk::Box,
    right: gtk::Box,
    browser: gtk::Box,
    notice: gtk::Box,
}

impl FilesControl {
    fn show_in_inspector(&self, right: bool) {
        let target = if right { &self.right } else { &self.home };
        if self.browser.parent().as_ref() != Some(target.upcast_ref::<gtk::Widget>()) {
            if let Some(parent) = self.browser.parent().and_downcast::<gtk::Box>() {
                parent.remove(&self.browser);
            }
            target.append(&self.browser);
        }
        self.notice.set_visible(right);
    }
}

pub(crate) struct SessionContext<'a> {
    pub private: bool,
    pub cwd: Option<&'a str>,
    pub namespace: &'a str,
    pub configured_shell: &'a str,
    pub presentation: &'a str,
    pub process: Option<&'a str>,
    pub last_exit: Option<i32>,
    pub duration_ms: Option<u64>,
    pub grid: (u16, u16),
    pub branch: Option<&'a str>,
    pub share_command_context: bool,
}

/// This is a local, bounded view of observed state. Private tabs never place
/// their directory, process or command result in the displayed/copyable text.
pub(crate) fn session_context_text(context: &SessionContext<'_>) -> String {
    if context.private {
        return "Private session\nDirectory, process and command details are hidden.".into();
    }
    let safe = |value: &str| jterm_core::review_input::safe_inline_display(value, 1024);
    let mut lines = vec![
        format!("Directory: {}", safe(context.cwd.unwrap_or("Not reported"))),
        format!("Filesystem: {}", safe(context.namespace)),
        format!("Configured shell: {}", safe(context.configured_shell)),
        format!("Presentation: {}", context.presentation),
        format!(
            "Foreground process: {}",
            safe(context.process.unwrap_or("Not reported"))
        ),
        format!("Terminal: {} × {}", context.grid.0, context.grid.1),
    ];
    if let Some(branch) = context.branch {
        lines.push(format!("Git branch: {}", safe(branch)));
    }
    if let Some(code) = context.last_exit {
        lines.push(format!("Last command exit: {code}"));
    }
    if let Some(duration) = context.duration_ms {
        lines.push(format!("Last command duration: {duration} ms"));
    }
    lines.push(format!(
        "Command context sharing: {}",
        if context.share_command_context {
            "Enabled by your settings"
        } else {
            "Off"
        }
    ));
    lines.join("\n")
}

fn update_session_context(
    state: &gtk::Label,
    copy: &gtk::Button,
    value: &std::cell::RefCell<String>,
    context: &SessionContext<'_>,
) {
    let summary = session_context_text(context);
    if state.text().as_str() != summary {
        state.set_label(&summary);
    }
    copy.set_sensitive(!context.private && !summary.is_empty());
    *value.borrow_mut() = if context.private {
        String::new()
    } else {
        summary
    };
}

/// The outer inspector is independent of the provider/task page it contains.
/// None retains the existing automatic collapse; an explicit Show also works
/// as a drawer on ordinary narrow windows. No page or terminal is recreated.
struct InspectorControl {
    requested: std::cell::Cell<Option<bool>>,
    shown: std::cell::Cell<bool>,
    available: std::cell::Cell<bool>,
    button: gtk::ToggleButton,
    refresh: std::cell::RefCell<Option<std::rc::Rc<dyn Fn()>>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SidebarEdge {
    Navigation,
    Inspector,
}

/// User preferences survive responsive clamps, hiding and drawer transitions.
/// Only native handle input commits widths; allocation notifications do not.
struct SidebarWidths {
    navigation: std::cell::Cell<u32>,
    inspector: std::cell::Cell<u32>,
    navigation_visible: std::cell::Cell<bool>,
    editing: std::cell::Cell<Option<SidebarEdge>>,
    edit_start: std::cell::Cell<Option<i32>>,
    drawer_preview: std::cell::Cell<Option<u32>>,
    applying: std::cell::Cell<bool>,
}

impl SidebarWidths {
    fn new(navigation: u32, inspector: u32, navigation_visible: bool) -> Self {
        Self {
            navigation: std::cell::Cell::new(navigation.clamp(
                tokens::MIN_NAVIGATION_WIDTH as u32,
                tokens::MAX_NAVIGATION_WIDTH as u32,
            )),
            inspector: std::cell::Cell::new(inspector.clamp(
                tokens::MIN_INSPECTOR_WIDTH as u32,
                tokens::MAX_INSPECTOR_WIDTH as u32,
            )),
            navigation_visible: std::cell::Cell::new(navigation_visible),
            editing: std::cell::Cell::new(None),
            edit_start: std::cell::Cell::new(None),
            drawer_preview: std::cell::Cell::new(None),
            applying: std::cell::Cell::new(false),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InspectorPlacement {
    Hidden,
    Docked,
    Drawer,
}

fn inspector_placement(
    layout: &tokens::Layout,
    requested: Option<bool>,
) -> (InspectorPlacement, bool) {
    let inline = layout.inspector.is_some();
    // Keep a small part of the canvas reachable beside a narrow drawer. The
    // actual panel remains its normal width and its own content can scroll.
    let drawer_fits = layout.titlebar.width
        >= tokens::RAIL_WIDTH + tokens::INSPECTOR_WIDTH + 2.0 * tokens::CONTROL_HEIGHT
        && layout.center.height >= tokens::INSPECTOR_TABS_HEIGHT + tokens::CONTROL_HEIGHT;
    let placement = if requested == Some(false) {
        InspectorPlacement::Hidden
    } else if inline {
        InspectorPlacement::Docked
    } else if requested == Some(true) && drawer_fits {
        InspectorPlacement::Drawer
    } else {
        InspectorPlacement::Hidden
    };
    (placement, inline || drawer_fits)
}

impl InspectorControl {
    fn sync_button(&self, placement: InspectorPlacement, available: bool) {
        let shown = placement != InspectorPlacement::Hidden;
        self.shown.set(shown);
        self.available.set(available);
        self.button.set_active(shown);
        self.button.set_sensitive(available);
        let title = if shown {
            "Hide right sidebar"
        } else {
            "Show right sidebar"
        };
        let description = if available {
            "Toggle the right inspector. Narrow windows use a sidebar drawer."
        } else {
            "The window is too small to fit the right sidebar. Enlarge it to show the sidebar."
        };
        self.button.set_tooltip_text(Some(&if available {
            title.into()
        } else {
            format!("{title}\n{description}")
        }));
        self.button.update_property(&[
            gtk::accessible::Property::Label(title),
            gtk::accessible::Property::Description(description),
        ]);
    }
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
    let size = jterm_core::reference_popup::FOOTER_ICON_SIZE;
    FIXED_CHROME_ICONS.with_borrow_mut(|entries| {
        entries.retain(|(icon, _)| icon.upgrade().is_some());
        entries.push((icon.downgrade(), size));
    });
    let size = (f64::from(size) * chrome_geometry_scale().min(1.25)).round() as i32;
    icon.set_content_width(size);
    icon.set_content_height(size);
    icon
}

fn chrome_geometry_scale() -> f64 {
    UI_ROLE_SCALE.with(|scale| scale.get().max(1.0))
}

fn fixed_chrome_width(widget: &impl IsA<gtk::Widget>, width: f32) {
    let widget = widget.as_ref();
    let height = widget
        .has_css_class("reference-composer-fixed")
        .then_some(COMPOSER_CONTROL_HEIGHT);
    FIXED_CHROME_WIDTHS.with_borrow_mut(|entries| {
        entries.retain(|(widget, _, _)| widget.upgrade().is_some());
        entries.push((widget.downgrade(), width, height));
    });
    widget.set_width_request((f64::from(width) * chrome_geometry_scale()).round() as i32);
    if height.is_some() {
        widget.set_height_request(composer_control_height(chrome_geometry_scale()));
        widget.set_valign(gtk::Align::Center);
        widget.set_overflow(gtk::Overflow::Hidden);
    }
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
    title.set_width_chars(1);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    row.set_tooltip_text(Some(text));
    content.append(&title);
    row.set_child(Some(&content));
    row.add_css_class("shell-navigation-row");
    // Row height and inter-row gap are separate shared measurements.
    row.set_height_request(tokens::NAVIGATION_ROW_HEIGHT as i32);
    row
}

/// Subtitle role names: `block` becomes `Block`, matching Forge's header.
fn presentation_title(presentation: &str) -> String {
    let mut chars = presentation.trim().chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => "Terminal".to_string(),
    }
}

/// Inspector overview action: a plain full-width button, as in Forge's
/// `reference-agent-actions`. Icons stay with navigation rows.
fn overview_action(text: &str, sender: &ComponentSender<AppModel>, msg: AppMsg) -> gtk::Button {
    let button = gtk::Button::with_label(text);
    button.set_tooltip_text(Some(text));
    let sender = sender.clone();
    button.connect_clicked(move |_| sender.input(msg.clone()));
    button
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
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let secondary = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let add = unavailable("Attach");
    add.add_css_class("chat-add");
    add.add_css_class("reference-composer-fixed");
    fixed_chrome_width(&add, tokens::COMPOSER_PLUS_WIDTH);
    add.set_child(Some(&footer_icon(Icon::Plus)));
    secondary.append(&add);
    let mut optional_tools = Vec::new();
    for (name, width) in [
        ("Attach", tokens::COMPOSER_ATTACH_WIDTH),
        ("Image", tokens::COMPOSER_IMAGE_WIDTH),
        ("Code", tokens::COMPOSER_CODE_WIDTH),
    ] {
        let button = unavailable(name);
        button.add_css_class("reference-composer-fixed");
        fixed_chrome_width(&button, width);
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        content.append(&footer_icon(Icon::from_key(&name.to_ascii_lowercase())));
        let title = gtk::Label::new(Some(name));
        title.set_width_chars(0);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        content.append(&title);
        button.set_child(Some(&content));
        optional_tools.push(button.downgrade());
        secondary.append(&button);
    }
    let terminal = gtk::Button::new();
    terminal.add_css_class("flat");
    terminal.add_css_class("chat-terminal-capsule");
    terminal.add_css_class("reference-composer-fixed");
    fixed_chrome_width(&terminal, tokens::COMPOSER_TERMINAL_WIDTH);
    terminal.set_focus_on_click(false);
    let terminal_content = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    terminal_content.append(&footer_icon(Icon::Terminal));
    let terminal_label = label("Terminal", "shell-canvas-subtitle");
    terminal_label.set_width_chars(0);
    terminal_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    terminal_content.append(&terminal_label);
    terminal.set_child(Some(&terminal_content));
    terminal.set_tooltip_text(Some("Focus the active terminal"));
    terminal.update_property(&[gtk::accessible::Property::Label("Focus terminal")]);
    let focus = focus_terminal.clone();
    terminal.connect_clicked(move |_| focus());
    secondary.append(&terminal);
    let secondary_scroll = gtk::ScrolledWindow::new();
    secondary_scroll.set_policy(gtk::PolicyType::External, gtk::PolicyType::Never);
    secondary_scroll.set_min_content_width(0);
    // A fixed 400px cap clips Terminal under Tools after UI font zoom.
    // The native child now supplies its complete, already-scaled width.
    secondary_scroll.set_max_content_width(i32::MAX);
    secondary_scroll.set_width_request(1);
    secondary_scroll.set_hexpand(false);
    secondary_scroll.set_propagate_natural_width(true);
    secondary_scroll.set_child(Some(&secondary));
    row.append(&secondary_scroll);
    let tools = gtk::MenuButton::new();
    tools.add_css_class("flat");
    tools.add_css_class("reference-tools-button");
    tools.add_css_class("reference-composer-fixed");
    fixed_chrome_width(&tools, tokens::COMPOSER_TOOLS_WIDTH);
    tools.set_margin_start(tokens::METADATA_GAP as i32);
    let tools_content = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    let tools_grid = footer_icon(Icon::Grid);
    tools_content.append(&tools_grid);
    let tools_label = label("Tools", "shell-tool-label");
    tools_label.set_width_chars(0);
    tools_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    tools_content.append(&tools_label);
    let chevron = footer_icon(Icon::ChevronDown);
    tools_content.append(&chevron);
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
    crate::block_view::bind_persistent_menu_focus(&tools, &popover);
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
    spacer.set_width_request(tokens::METADATA_GAP as i32);
    row.append(&spacer);
    let model = unavailable(tokens::MODEL_UNAVAILABLE_LABEL);
    model.add_css_class("chat-tool-capsule");
    model.add_css_class("shell-composer-model");
    model.add_css_class("reference-composer-fixed");
    fixed_chrome_width(&model, tokens::COMPOSER_MODEL_MIN_WIDTH);
    model.set_margin_end(tokens::METADATA_GAP as i32);
    model.set_tooltip_text(Some("No agent model is connected to this shell input"));
    row.append(&model);
    send.add_css_class("chat-send");
    send.add_css_class("reference-composer-fixed");
    fixed_chrome_width(send, tokens::COMPOSER_SEND_WIDTH);
    send.set_child(Some(&footer_icon(Icon::Send)));
    row.append(send);
    if let Some(stop) = stop {
        stop.add_css_class("reference-composer-fixed");
        fixed_chrome_width(stop, tokens::COMPOSER_SEND_WIDTH);
        row.append(stop);
    }
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&row));
    let observer = gtk::DrawingArea::new();
    observer.set_can_target(false);
    observer.set_focusable(false);
    overlay.add_overlay(&observer);
    overlay.set_measure_overlay(&observer, false);
    let terminal = terminal.downgrade();
    let model = model.downgrade();
    observer.connect_resize(move |observer, width, _| {
        // Compact thresholds refer to the whole composer border box, not the
        // tool row after padding. UI zoom uses the same role ratio; DPR is
        // already handled by GTK and is never applied a second time here.
        let mut ancestor = observer.parent();
        let mut available = f64::from(width);
        while let Some(widget) = ancestor {
            if widget.has_css_class("chat-composer") || widget.has_css_class("shell-block-composer")
            {
                if let Some(bounds) = widget.compute_bounds(&widget) {
                    available = f64::from(bounds.width());
                }
                break;
            }
            ancestor = widget.parent();
        }
        let available = available / chrome_geometry_scale();
        for widget in &optional_tools {
            if let Some(widget) = widget.upgrade() {
                widget.set_visible(available >= f64::from(tokens::COMPOSER_SECONDARY_MIN_WIDTH));
            }
        }
        if let Some(model) = model.upgrade() {
            model.set_visible(available >= f64::from(tokens::COMPOSER_MODEL_MIN_AVAILABLE));
        }
        if let Some(terminal) = terminal.upgrade() {
            // Its existing focus callback is also reachable in Tools.
            terminal.set_visible(available >= f64::from(tokens::COMPOSER_TERMINAL_MIN_AVAILABLE));
        }
    });
    let controls = gtk::Box::new(gtk::Orientation::Vertical, 0);
    controls.add_css_class("chat-controls");
    controls.add_css_class("reference-composer-controls");
    own_composer_chrome(&controls);
    controls.set_height_request(composer_control_height(chrome_geometry_scale()));
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
        config: &Config,
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
        modes.set_height_request(tokens::MODE_CONTROL_HEIGHT as i32);
        let mut mode_buttons = Vec::new();
        for (title, index) in [("Block", 0), ("Chat", 1)] {
            let mode_button = gtk::Button::with_label(title);
            mode_button.add_css_class("flat");
            fixed_chrome_width(&mode_button, tokens::MODE_CONTROL_MIN_WIDTH);
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
        let inspector_button = gtk::ToggleButton::new();
        inspector_button.add_css_class("flat");
        inspector_button.add_css_class("shell-inspector-toggle");
        inspector_button.set_size_request(28, 28);
        inspector_button.set_valign(gtk::Align::Center);
        inspector_button.set_focus_on_click(false);
        inspector_button.set_child(Some(&icon(jterm_core::reference_icons::Icon::PanelRight)));
        let inspector_sender = sender.clone();
        inspector_button.connect_clicked(move |_| {
            inspector_sender.input(AppMsg::ToggleInspector);
        });
        let inspector_control = std::rc::Rc::new(InspectorControl {
            requested: std::cell::Cell::new(None),
            shown: std::cell::Cell::new(true),
            available: std::cell::Cell::new(true),
            button: inspector_button,
            refresh: std::cell::RefCell::new(None),
        });
        inspector_control.sync_button(InspectorPlacement::Docked, true);
        top.append(&inspector_control.button);

        // The launcher is centered independently of the left/right controls;
        // extra window width belongs to the canvas, not an expanding search.
        top.remove(&search);
        let title_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        title_spacer.set_hexpand(true);
        top.insert_child_after(&title_spacer, Some(&path));
        search.set_hexpand(false);
        search.set_width_request(tokens::SEARCH_WIDTH as i32);
        search.set_height_request(tokens::SEARCH_HEIGHT as i32);
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
        let title_allocation = title_overlay.downgrade();
        let title_visibility_epoch = std::rc::Rc::new(std::cell::Cell::new(0_u64));
        title_observer.connect_resize(move |observer, _, _| {
            let epoch = title_visibility_epoch.get().wrapping_add(1);
            title_visibility_epoch.set(epoch);
            let current_epoch = title_visibility_epoch.clone();
            let observer = observer.downgrade();
            let search = search_width_gate.clone();
            let path = path_width_gate.clone();
            let title = title_allocation.clone();
            // The observer is itself a non-measuring overlay. Changing a
            // sibling's visibility during this allocation can leave that
            // sibling mapped with its previous zero allocation. Apply only
            // the latest resize outside layout, then allocate the real host.
            gtk::glib::idle_add_local_once(move || {
                if current_epoch.get() != epoch {
                    return;
                }
                let (Some(observer), Some(title)) = (observer.upgrade(), title.upgrade()) else {
                    return;
                };
                let width = observer.width();
                if width <= 0 {
                    return;
                }
                let mut reallocate = false;
                if let Some(search) = search.upgrade() {
                    let visible = width >= 1024;
                    reallocate |= search.is_visible() != visible
                        || (visible && (search.width() <= 0 || search.height() <= 0));
                    search.set_visible(visible);
                }
                if let Some(path) = path.upgrade() {
                    let visible = width >= 1120;
                    reallocate |= path.is_visible() != visible;
                    path.set_visible(visible);
                }
                if reallocate {
                    title.queue_allocate();
                }
            });
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
        navigation.set_width_request(-1);
        let navigation_heading = gtk::Box::new(
            gtk::Orientation::Vertical,
            tokens::NAVIGATION_ROW_GAP as i32,
        );
        navigation_heading.append(&label("WORKSPACE", "shell-section"));
        let workspace = gtk::MenuButton::new();
        workspace.set_tooltip_text(Some("Switch active workspace session"));
        workspace.add_css_class("shell-navigation-row");
        workspace.add_css_class("flat");
        let workspace_menu = gtk::Box::new(gtk::Orientation::Vertical, 4);
        workspace_menu.set_width_request(240);
        let workspace_popover = gtk::Popover::new();
        let workspace_scroll = gtk::ScrolledWindow::new();
        workspace_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        workspace_scroll.set_max_content_height(360);
        workspace_scroll.set_propagate_natural_height(true);
        workspace_scroll.set_child(Some(&workspace_menu));
        workspace_popover.set_child(Some(&workspace_scroll));
        workspace.set_popover(Some(&workspace_popover));
        // This selector lists live session identities, so switching preserves
        // each shell, split layout and draft.
        workspace.add_css_class("shell-workspace-selector");
        let selector = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        selector.append(&icon(jterm_core::reference_icons::Icon::Files));
        let workspace_name = label("Local workspace", "shell-navigation-title");
        workspace_name.set_hexpand(true);
        workspace_name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        selector.append(&workspace_name);
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
            text.set_hexpand(true);
            let name_line = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            name_line.append(&label(name, "shell-navigation-title"));
            let availability = label("Unavailable", "shell-muted");
            availability.set_width_chars(1);
            availability.set_ellipsize(gtk::pango::EllipsizeMode::End);
            name_line.append(&availability);
            text.append(&name_line);
            let detail = label(description, "shell-muted");
            detail.set_width_chars(1);
            detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
            text.append(&detail);
            row.set_tooltip_text(Some(&format!("{name} · {description} · Unavailable")));
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
        let title = label("Workspace  /  Terminal", "shell-canvas-title");
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let mode = label("Block session · Local", "shell-canvas-subtitle");
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
        inspector.set_width_request(tokens::MIN_INSPECTOR_WIDTH as i32);
        let inspector_tabs = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        inspector_tabs.add_css_class("shell-inspector-tabs");
        inspector_tabs.set_height_request(tokens::INSPECTOR_TABS_HEIGHT as i32);
        let panel = gtk::Stack::new();
        panel.add_css_class("shell-inspector-pages");
        panel.set_hhomogeneous(false);
        panel.set_vhomogeneous(false);
        panel.set_vexpand(true);
        let fixture = jterm_core::reference_conversation::fixture_enabled();
        let inspector_model = if fixture {
            jterm_core::reference_conversation::acceptance_fixture().inspector
        } else {
            jterm_core::reference_conversation::Inspector::empty()
        };
        let overview_host = gtk::Box::new(gtk::Orientation::Vertical, 8);
        overview_host.set_vexpand(true);
        overview_host.append(&crate::workspace_conversation::inspector(
            &inspector_model,
            fixture,
        ));
        let overview_actions = gtk::Box::new(gtk::Orientation::Vertical, 6);
        overview_actions.add_css_class("reference-agent-actions");
        overview_actions.set_visible(!fixture);
        if !fixture {
            overview_actions.append(&overview_action(
                "New agent task",
                sender,
                AppMsg::TaskPanelAction(crate::agent_task_ui::TaskPanelAction::ComposeProject),
            ));
            overview_actions.append(&overview_action(
                "Manage agent tasks",
                sender,
                AppMsg::Action(Action::ToggleTasksPanel),
            ));
            overview_actions.append(&overview_action(
                "Open AI assistant",
                sender,
                AppMsg::Action(Action::OpenAiPanel),
            ));
            overview_actions.append(&overview_action(
                "Saved agent sessions",
                sender,
                AppMsg::Action(Action::OpenAgentSessions),
            ));
        }
        let overview = gtk::Box::new(gtk::Orientation::Vertical, 8);
        overview.append(&overview_actions);
        overview.append(&overview_host);
        let overview_scroll = gtk::ScrolledWindow::new();
        overview_scroll.add_css_class("reference-inspector-scroll");
        overview_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        overview_scroll.set_vexpand(true);
        overview_scroll.set_child(Some(&overview));
        panel.add_named(&overview_scroll, Some("overview"));
        // Keep legacy provider/task controls accessible without letting their
        // natural minimum replace the shared inspector width. This one host
        // and the original live stack survive every dock/drawer transition.
        let live_host = gtk::ScrolledWindow::new();
        live_host.add_css_class("shell-inspector-live-scroll");
        live_host.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
        live_host.set_propagate_natural_width(false);
        live_host.set_propagate_natural_height(false);
        live_host.set_hexpand(true);
        live_host.set_vexpand(true);
        live_host.set_child(Some(side_stack));
        panel.add_named(&live_host, Some("live"));
        let context_page = gtk::Box::new(gtk::Orientation::Vertical, 12);
        context_page.add_css_class("shell-agent-card");
        context_page.append(&label("SESSION CONTEXT", "shell-section"));
        let context_details = label("Local terminal", "shell-inspector-path");
        context_details.set_wrap(true);
        context_details.set_selectable(true);
        context_page.append(&context_details);
        let context_state = label("No active session", "shell-muted");
        context_state.set_wrap(true);
        context_state.set_selectable(true);
        context_page.append(&context_state);
        let context_value = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        let context_copy = gtk::Button::with_label("Copy session context");
        let copy_value = context_value.clone();
        context_copy
            .connect_clicked(move |button| button.clipboard().set_text(&copy_value.borrow()));
        context_page.append(&context_copy);
        let browse = action_row(
            "folder",
            "Browse session directory",
            sender,
            AppMsg::FileTreeGotoCwd,
        );
        let browse_panel = panel.downgrade();
        browse.connect_clicked(move |_| {
            if let Some(panel) = browse_panel.upgrade() {
                panel.set_visible_child_name("files");
            }
        });
        context_page.append(&browse);
        context_page.append(&action_row(
            "terminal",
            "Focus terminal",
            sender,
            AppMsg::FocusShell,
        ));
        context_page.append(&action_row(
            "search",
            "Search terminal output",
            sender,
            AppMsg::Action(Action::ToggleSearch),
        ));
        context_page.append(&action_row(
            "settings",
            "Context sharing settings",
            sender,
            AppMsg::Action(Action::ToggleSettings),
        ));
        let context_notice = label("No terminal content is sent automatically. Agent context sharing follows your consent settings.", "shell-muted");
        context_notice.set_wrap(true);
        context_page.append(&context_notice);
        context_page.set_valign(gtk::Align::Start);
        let context_scroll = gtk::ScrolledWindow::new();
        context_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        context_scroll.set_vexpand(true);
        context_scroll.set_child(Some(&context_page));
        panel.add_named(&context_scroll, Some("context"));
        panel.set_visible_child_name(if side_stack.is_visible() {
            "live"
        } else {
            "overview"
        });
        let live_panel = panel.downgrade();
        side_stack.connect_visible_notify(move |stack| {
            if let Some(panel) = live_panel.upgrade() {
                if stack.is_visible() {
                    panel.set_visible_child_name("live");
                } else if panel.visible_child_name().as_deref() == Some("live") {
                    panel.set_visible_child_name("overview");
                }
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
            let panel = panel.downgrade();
            tab.connect_clicked(move |_| {
                if let Some(panel) = panel.upgrade() {
                    panel.set_visible_child_name(page);
                }
            });
            inspector_tabs.append(tab);
        }
        let files_tab = gtk::Button::with_label("Files");
        files_tab.add_css_class("flat");
        files_tab.set_tooltip_text(Some(
            "Browse the current Files location in the right sidebar",
        ));
        let files_panel = panel.downgrade();
        files_tab.connect_clicked(move |_| {
            if let Some(panel) = files_panel.upgrade() {
                panel.set_visible_child_name("files");
            }
        });
        let agent = agent_tab.downgrade();
        let context = context_tab.downgrade();
        let files = files_tab.downgrade();
        panel.connect_visible_child_name_notify(move |panel| {
            let context_selected = panel.visible_child_name().as_deref() == Some("context");
            let files_selected = panel.visible_child_name().as_deref() == Some("files");
            if let Some(agent) = agent.upgrade() {
                if context_selected || files_selected {
                    agent.remove_css_class("shell-selected");
                } else {
                    agent.add_css_class("shell-selected");
                }
            }
            if let Some(context) = context.upgrade() {
                if context_selected {
                    context.add_css_class("shell-selected");
                } else {
                    context.remove_css_class("shell-selected");
                }
            }
            if let Some(files) = files.upgrade() {
                if files_selected {
                    files.add_css_class("shell-selected");
                } else {
                    files.remove_css_class("shell-selected");
                }
            }
        });
        files_tab.set_width_request(tokens::INSPECTOR_TAB_WIDTH as i32);
        files_tab.set_valign(gtk::Align::Center);
        files_tab.set_height_request(28);
        inspector_tabs.append(&files_tab);
        let tab_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        tab_spacer.set_hexpand(true);
        inspector_tabs.append(&tab_spacer);
        let inspector_options = gtk::MenuButton::new();
        inspector_options.add_css_class("flat");
        inspector_options.set_tooltip_text(Some("Inspector options"));
        let options = gtk::Popover::new();
        let options_list = gtk::Box::new(gtk::Orientation::Vertical, 4);
        for (title, msg) in [
            (
                "Manage agent tasks",
                AppMsg::Action(Action::ToggleTasksPanel),
            ),
            ("Open AI assistant", AppMsg::Action(Action::OpenAiPanel)),
            (
                "Saved agent sessions",
                AppMsg::Action(Action::OpenAgentSessions),
            ),
            ("Settings", AppMsg::Action(Action::ToggleSettings)),
            ("Hide right sidebar", AppMsg::ToggleInspector),
        ] {
            let option = action_row("settings", title, sender, msg);
            let options = options.clone();
            option.connect_clicked(move |_| options.popdown());
            options_list.append(&option);
        }
        options.set_child(Some(&options_list));
        inspector_options.set_popover(Some(&options));
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
            context_state,
            context_copy,
            context_value,
            workspace_menu,
            workspace_popover,
            workspace_entries: std::cell::RefCell::new(Vec::new()),
            overview_host,
            overview_model: std::cell::RefCell::new(None),
            overview_actions,
            overview_action_state: std::cell::RefCell::new(Vec::new()),
            task_actions_available: std::cell::Cell::new(None),
            sender: sender.clone(),
            files_control: std::cell::RefCell::new(None),
            block_shelf,
            presentation_navigation: presentation_navigation
                .expect("presentation navigation label"),
            search,
            inspector_control,
            inspector_pages: panel,
            sidebar_widths: std::rc::Rc::new(SidebarWidths::new(
                config.sidebar_width,
                config.ai_panel_width,
                config.sidebar_visible,
            )),
        }
    }

    pub(crate) fn connect_files_browser(&self, home: &gtk::Box) {
        let browser = gtk::Box::new(gtk::Orientation::Vertical, 0);
        browser.set_vexpand(true);
        while let Some(child) = home.first_child() {
            home.remove(&child);
            browser.append(&child);
        }
        let right = gtk::Box::new(gtk::Orientation::Vertical, 0);
        right.set_vexpand(true);
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
        scroll.set_propagate_natural_width(false);
        scroll.set_vexpand(true);
        scroll.set_child(Some(&right));
        self.inspector_pages.add_named(&scroll, Some("files"));
        let notice = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let message = label("Files are open in the right sidebar", "shell-muted");
        message.set_wrap(true);
        notice.append(&message);
        notice.append(&action_row(
            "folder",
            "Show files here",
            &self.sender,
            AppMsg::SetSidebarView(crate::config::SidebarView::Files),
        ));
        home.append(&notice);
        let control = std::rc::Rc::new(FilesControl {
            home: home.clone(),
            right,
            browser,
            notice,
        });
        control.show_in_inspector(false);
        let moved = control.clone();
        let sender = self.sender.clone();
        self.inspector_pages
            .connect_visible_child_name_notify(move |panel| {
                moved.show_in_inspector(panel.visible_child_name().as_deref() == Some("files"));
                if panel.visible_child_name().as_deref() == Some("files") {
                    sender.input(AppMsg::FileTreeTtlTick);
                }
            });
        *self.files_control.borrow_mut() = Some(control);
    }

    pub(crate) fn show_files_navigation(&self) {
        if self.inspector_pages.visible_child_name().as_deref() == Some("files") {
            self.inspector_pages.set_visible_child_name("overview");
        }
        if let Some(control) = self.files_control.borrow().as_ref() {
            control.show_in_inspector(false);
        }
    }

    pub(crate) fn sync_session_context(&self, context: &SessionContext<'_>) {
        update_session_context(
            &self.context_state,
            &self.context_copy,
            &self.context_value,
            context,
        );
        if !jterm_core::reference_conversation::fixture_enabled() {
            self.mode.set_label(&format!(
                "{} session · {}",
                presentation_title(context.presentation),
                if context.private {
                    "Private"
                } else {
                    context.namespace
                }
            ));
        }
    }

    pub(crate) fn sync_workspace_entries(&self, entries: &[(u64, String, bool)]) {
        if self.workspace_entries.borrow().as_slice() == entries {
            return;
        }
        *self.workspace_entries.borrow_mut() = entries.to_vec();
        while let Some(child) = self.workspace_menu.first_child() {
            self.workspace_menu.remove(&child);
        }
        self.workspace_menu
            .append(&label("OPEN SESSIONS", "shell-section"));
        for (id, title, selected) in entries {
            let row = action_row("terminal", title, &self.sender, AppMsg::SelectTab(*id));
            if *selected {
                row.add_css_class("shell-selected");
            }
            let popover = self.workspace_popover.clone();
            row.connect_clicked(move |_| popover.popdown());
            self.workspace_menu.append(&row);
        }
        for (title, msg) in [
            ("New session", AppMsg::NewTab),
            ("Connect remote…", AppMsg::Action(Action::ShowRemotePicker)),
        ] {
            let row = action_row("workspace", title, &self.sender, msg);
            let popover = self.workspace_popover.clone();
            row.connect_clicked(move |_| popover.popdown());
            self.workspace_menu.append(&row);
        }
    }

    pub(crate) fn sync_task_overview(
        &self,
        model: &jterm_core::reference_conversation::Inspector,
        detail: Option<&crate::dialogs::tasks_panel::TaskDetailSync>,
        allow_create: bool,
        allow_manage: bool,
    ) {
        if jterm_core::reference_conversation::fixture_enabled() {
            return;
        }
        if self.overview_model.borrow().as_ref() != Some(model) {
            *self.overview_model.borrow_mut() = Some(model.clone());
            while let Some(child) = self.overview_host.first_child() {
                self.overview_host.remove(&child);
            }
            self.overview_host
                .append(&crate::workspace_conversation::inspector(model, false));
        }
        use crate::agent_task_ui::TaskPanelAction;
        let mut actions = Vec::new();
        if let Some(detail) = detail.filter(|detail| !detail.archived && allow_manage) {
            actions.push((
                "Manage selected task".into(),
                TaskPanelAction::Select(detail.id),
            ));
            if detail.can_start_native && allow_create {
                actions.push((
                    "Start native agent".into(),
                    TaskPanelAction::StartNative(detail.id),
                ));
            }
            if detail.can_start_terminal && allow_create {
                actions.push((
                    "Start agent in terminal".into(),
                    TaskPanelAction::StartTerminal(detail.id),
                ));
            }
            if detail.can_focus_terminal {
                actions.push((
                    "Open agent terminal".into(),
                    TaskPanelAction::FocusTerminal(detail.id),
                ));
            }
            if detail.can_focus_validation {
                actions.push((
                    "Open validation output".into(),
                    TaskPanelAction::FocusValidation(detail.id),
                ));
            }
            if detail.can_run_validation && allow_create {
                actions.push((
                    "Run validation".into(),
                    TaskPanelAction::RunValidation(detail.id),
                ));
            }
            actions.push((
                "Review task changes".into(),
                TaskPanelAction::ReviewDiff(detail.id),
            ));
        }
        if *self.overview_action_state.borrow() == actions
            && self.task_actions_available.get() == Some((allow_create, allow_manage))
        {
            return;
        }
        self.task_actions_available
            .set(Some((allow_create, allow_manage)));
        *self.overview_action_state.borrow_mut() = actions.clone();
        while let Some(child) = self.overview_actions.first_child() {
            self.overview_actions.remove(&child);
        }
        for (title, action) in actions {
            let row = gtk::Button::with_label(&title);
            let sender = self.sender.clone();
            let selected = detail.map(|detail| detail.id);
            row.connect_clicked(move |_| {
                sender.input(AppMsg::Action(Action::ToggleTasksPanel));
                if let Some(id) = selected {
                    sender.input(AppMsg::TaskPanelAction(TaskPanelAction::Select(id)));
                }
                sender.input(AppMsg::TaskPanelAction(action.clone()));
            });
            self.overview_actions.append(&row);
        }
        let create = overview_action(
            "New agent task",
            &self.sender,
            AppMsg::TaskPanelAction(TaskPanelAction::ComposeProject),
        );
        create.set_sensitive(allow_create);
        create.set_tooltip_text(Some(if allow_create {
            "Create an isolated task with an explicit goal"
        } else {
            "Agent tasks must be enabled in configuration and require normal mode"
        }));
        self.overview_actions.append(&create);
        let manage = overview_action(
            "Manage agent tasks",
            &self.sender,
            AppMsg::Action(Action::ToggleTasksPanel),
        );
        manage.set_sensitive(allow_manage);
        self.overview_actions.append(&manage);
        self.overview_actions.append(&overview_action(
            "Open AI assistant",
            &self.sender,
            AppMsg::Action(Action::OpenAiPanel),
        ));
        self.overview_actions.append(&overview_action(
            "Saved agent sessions",
            &self.sender,
            AppMsg::Action(Action::OpenAgentSessions),
        ));
    }

    pub(crate) fn toggle_inspector(&self) {
        if !self.inspector_control.available.get() {
            return;
        }
        self.inspector_control
            .requested
            .set(Some(!self.inspector_control.shown.get()));
        self.refresh_inspector_layout();
    }

    pub(crate) fn inspector_is_shown(&self) -> bool {
        self.inspector_control.shown.get()
    }

    pub(crate) fn inspector_live_is_selected(&self) -> bool {
        self.inspector_pages.visible_child_name().as_deref() == Some("live")
    }

    pub(crate) fn show_inspector_live(&self) {
        self.inspector_control.requested.set(Some(true));
        self.inspector_pages.set_visible_child_name("live");
        self.refresh_inspector_layout();
    }

    pub(crate) fn sidebar_widths(&self) -> (u32, u32) {
        (
            self.sidebar_widths.navigation.get(),
            self.sidebar_widths.inspector.get(),
        )
    }

    pub(crate) fn set_sidebar_widths(&self, navigation: u32, inspector: u32) {
        self.sidebar_widths.navigation.set(navigation.clamp(
            tokens::MIN_NAVIGATION_WIDTH as u32,
            tokens::MAX_NAVIGATION_WIDTH as u32,
        ));
        self.sidebar_widths.inspector.set(inspector.clamp(
            tokens::MIN_INSPECTOR_WIDTH as u32,
            tokens::MAX_INSPECTOR_WIDTH as u32,
        ));
        self.refresh_inspector_layout();
    }

    pub(crate) fn set_navigation_visible(&self, visible: bool) {
        self.sidebar_widths.navigation_visible.set(visible);
        self.refresh_inspector_layout();
    }

    pub(crate) fn refresh_inspector_layout(&self) {
        let refresh = self.inspector_control.refresh.borrow().clone();
        if let Some(refresh) = refresh {
            refresh();
        }
    }

    pub(crate) fn sync(&self, title: &str, path: &str, mode: TerminalMode, private: bool) {
        if log::log_enabled!(target: "anvil::workspace_shell", log::Level::Debug) {
            if let Some(bounds) = self.search.compute_bounds(&self.top) {
                log::debug!(target: "anvil::workspace_shell", "launcher_geometry x={} y={} width={} height={}", bounds.x(), bounds.y(), bounds.width(), bounds.height());
            }
        }
        let path = if private { "Private session" } else { path };
        // Same breadcrumb as Forge's canvas header.
        let heading = format!("Workspace  /  {title}");
        self.title.set_label(&heading);
        self.title.set_tooltip_text(Some(&heading));
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
        // Block and Chat own their real per-pane input dock; the outer shelf
        // is only for the traditional native terminal presentation.
        self.block_shelf
            .set_visible(!matches!(mode, TerminalMode::Block | TerminalMode::Chat));
        self.canvas_header
            .set_height_request(tokens::CANVAS_HEADER_HEIGHT as i32);
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
            self.title.set_tooltip_text(Some(&fixture.title));
            self.mode
                .set_label(jterm_core::reference_conversation::FIXTURE_LABEL);
        } else {
            self.mode.set_label(&format!("{mode} session · Local"));
        }
    }
}

/// Paint-only contract. Native terminal and canvas geometry remain backend-owned.
pub(crate) fn install_css() {
    let css = include_str!("workspace_shell.css");
    let provider = gtk::CssProvider::new();
    provider.load_from_string(&format!("{css}\n{}", include_str!("workspace_popups.css")));
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 20,
        );
    }
}

type FixedChromeWidth = (gtk::glib::WeakRef<gtk::Widget>, f32, Option<f32>);

thread_local! {
    static UI_ROLE_SCALE: std::cell::Cell<f64> = const { std::cell::Cell::new(1.0) };
    static FIXED_CHROME_WIDTHS: std::cell::RefCell<Vec<FixedChromeWidth>> = const {
        std::cell::RefCell::new(Vec::new())
    };
    static FIXED_CHROME_ICONS: std::cell::RefCell<Vec<(gtk::glib::WeakRef<gtk::DrawingArea>, f32)>> = const {
        std::cell::RefCell::new(Vec::new())
    };
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
    UI_ROLE_SCALE.with(|value| {
        value.set(if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        })
    });
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
    let widths = FIXED_CHROME_WIDTHS.with_borrow_mut(|entries| {
        entries.retain(|(widget, _, _)| widget.upgrade().is_some());
        entries
            .iter()
            .filter_map(|(widget, width, height)| {
                widget.upgrade().map(|widget| (widget, *width, *height))
            })
            .collect::<Vec<_>>()
    });
    // Registry borrows end before setters can cause native size notifications.
    for (widget, width, height) in widths {
        widget.set_width_request((f64::from(width) * chrome_geometry_scale()).round() as i32);
        if height.is_some() {
            widget.set_height_request(composer_control_height(chrome_geometry_scale()));
        }
    }
    let icons = FIXED_CHROME_ICONS.with_borrow_mut(|entries| {
        entries.retain(|(icon, _)| icon.upgrade().is_some());
        entries
            .iter()
            .filter_map(|(icon, size)| icon.upgrade().map(|icon| (icon, *size)))
            .collect::<Vec<_>>()
    });
    for (icon, size) in icons {
        let size = (f64::from(size) * chrome_geometry_scale().min(1.25)).round() as i32;
        icon.set_content_width(size);
        icon.set_content_height(size);
    }
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

/// Native Paned positions are relative to their current allocations. A parent
/// resize can reach the window observer before the nested right pane has grown;
/// ask for its desired end-content width, never a future center position.
fn apply_sidebar_positions(
    navigation: &gtk::Paned,
    inspector: &gtk::Paned,
    layout: &tokens::Layout,
    widths: &SidebarWidths,
) {
    if widths.editing.get().is_some() {
        return;
    }
    widths.applying.set(true);
    navigation.set_position(layout.navigation.map_or(0, |rect| rect.width as i32));
    let right_position = layout.inspector.map_or(inspector.width(), |rect| {
        (inspector.width() - rect.width as i32 - tokens::SIDEBAR_DIVIDER_WIDTH as i32).max(0)
    });
    inspector.set_position(right_position);
    widths.applying.set(false);
}

/// Reconcile once after native allocation/minimum changes. Coalescing avoids
/// setters during the allocation that emitted notify and needs no frame timer.
fn observe_sidebar_allocations(panes: &[&gtk::Paned], refresh: std::rc::Rc<dyn Fn()>) {
    let pending = std::rc::Rc::new(std::cell::Cell::new(false));
    let queue: std::rc::Rc<dyn Fn()> = std::rc::Rc::new(move || {
        if pending.replace(true) {
            return;
        }
        let pending = pending.clone();
        let refresh = refresh.clone();
        gtk::glib::idle_add_local_once(move || {
            pending.set(false);
            refresh();
        });
    });
    for pane in panes {
        let changed = queue.clone();
        pane.connect_max_position_notify(move |_| changed());
        let changed = queue.clone();
        pane.connect_min_position_notify(move |_| changed());
    }
}

/// Observe native splitter input without claiming its gesture or rerouting any
/// terminal key. GTK retains pointer grabs and its F8/arrow/Enter/Escape actions.
fn install_sidebar_handle(
    paned: &gtk::Paned,
    edge: SidebarEdge,
    widths: &std::rc::Rc<SidebarWidths>,
    changed: std::rc::Rc<dyn Fn(SidebarEdge, u32)>,
    refresh: std::rc::Rc<dyn Fn()>,
) {
    paned.add_css_class("workspace-sidebar-split");
    paned.set_wide_handle(true);
    paned.set_focusable(true);
    paned.set_focus_on_click(false);
    let name = if edge == SidebarEdge::Navigation {
        "Resize left sidebar"
    } else {
        "Resize right sidebar"
    };
    paned.update_property(&[
        gtk::accessible::Property::Label(name),
        gtk::accessible::Property::Description("Drag the divider to resize. F8 focuses dividers; arrow keys resize, Enter accepts, and Escape cancels."),
    ]);
    let mut child = paned.first_child();
    while let Some(widget) = child {
        child = widget.next_sibling();
        if widget.css_name() == "separator" {
            widget.set_cursor_from_name(Some("col-resize"));
            widget.set_tooltip_text(Some(&format!("{name} · Drag, or F8 then arrow keys")));
        }
    }
    let finish: std::rc::Rc<dyn Fn()> = {
        let paned = paned.downgrade();
        let widths = widths.clone();
        let changed = changed.clone();
        let refresh = refresh.clone();
        std::rc::Rc::new(move || {
            if widths.editing.get() != Some(edge) {
                return;
            }
            widths.editing.set(None);
            let start_position = widths.edit_start.take();
            let paned = paned.clone();
            let widths = widths.clone();
            let changed = changed.clone();
            let refresh = refresh.clone();
            gtk::glib::idle_add_local_once(move || {
                let Some(paned) = paned.upgrade() else {
                    return;
                };
                if !paned.is_mapped() || widths.editing.get().is_some() {
                    return;
                }
                if start_position == Some(paned.position()) {
                    refresh();
                    return;
                }
                let width = if edge == SidebarEdge::Navigation {
                    paned.position().clamp(
                        tokens::MIN_NAVIGATION_WIDTH as i32,
                        tokens::MAX_NAVIGATION_WIDTH as i32,
                    ) as u32
                } else {
                    (paned.width() - paned.position() - tokens::SIDEBAR_DIVIDER_WIDTH as i32).clamp(
                        tokens::MIN_INSPECTOR_WIDTH as i32,
                        tokens::MAX_INSPECTOR_WIDTH as i32,
                    ) as u32
                };
                if edge == SidebarEdge::Navigation {
                    widths.navigation.set(width);
                } else {
                    widths.inspector.set(width);
                }
                changed(edge, width);
                refresh();
            });
        })
    };
    let input = gtk::EventControllerLegacy::new();
    input.set_propagation_phase(gtk::PropagationPhase::Capture);
    input.set_propagation_limit(gtk::PropagationLimit::SameNative);
    let observed = paned.downgrade();
    let edit = widths.clone();
    let released = finish.clone();
    let cancelled = refresh.clone();
    input.connect_event(move |_, event| {
        if matches!(
            event.event_type(),
            gtk::gdk::EventType::GrabBroken | gtk::gdk::EventType::TouchCancel
        ) {
            if edit.editing.get() == Some(edge) {
                edit.editing.set(None);
                edit.edit_start.set(None);
                let cancelled = cancelled.clone();
                gtk::glib::idle_add_local_once(move || cancelled());
            }
            return gtk::glib::Propagation::Proceed;
        }
        if let Some(button) = event.downcast_ref::<gtk::gdk::ButtonEvent>() {
            if button.button() == 1 {
                match event.event_type() {
                    gtk::gdk::EventType::ButtonPress => {
                        if let (Some(paned), Some((x, y))) = (observed.upgrade(), event.position())
                        {
                            if let Some(window) = paned.root().and_downcast::<gtk::Window>() {
                                if event.surface() != window.surface() {
                                    return gtk::glib::Propagation::Proceed;
                                }
                                if let Some(origin) = paned
                                    .compute_point(&window, &gtk::graphene::Point::new(0.0, 0.0))
                                {
                                    // GDK positions include native CSD/shadow insets;
                                    // compute_point is in the Window widget's coordinates.
                                    let (surface_x, surface_y) = window.surface_transform();
                                    let mut picked = paned.pick(
                                        x - surface_x - f64::from(origin.x()),
                                        y - surface_y - f64::from(origin.y()),
                                        gtk::PickFlags::DEFAULT,
                                    );
                                    while let Some(widget) = picked {
                                        if widget.css_name() == "separator"
                                            && widget.parent().as_ref()
                                                == Some(paned.upcast_ref::<gtk::Widget>())
                                        {
                                            edit.edit_start.set(Some(paned.position()));
                                            edit.editing.set(Some(edge));
                                            break;
                                        }
                                        if widget == *paned.upcast_ref::<gtk::Widget>() {
                                            break;
                                        }
                                        picked = widget.parent();
                                    }
                                }
                            }
                        }
                    }
                    gtk::gdk::EventType::ButtonRelease => released(),
                    _ => {}
                }
            }
        }
        gtk::glib::Propagation::Proceed
    });
    paned.add_controller(input);
    let edit = widths.clone();
    paned.connect_move_handle(move |paned, _| {
        if edit.editing.get() != Some(edge) {
            edit.edit_start.set(Some(paned.position()));
            edit.editing.set(Some(edge));
        }
        false
    });
    let accepted = finish.clone();
    paned.connect_accept_position(move |_| {
        accepted();
        false
    });
    let accepted = finish.clone();
    paned.connect_cycle_handle_focus(move |_, _| {
        accepted();
        false
    });
    let accepted = finish.clone();
    paned.connect_toggle_handle_focus(move |_| {
        accepted();
        false
    });
    let accepted = finish.clone();
    paned.connect_has_focus_notify(move |paned| {
        if !paned.has_focus() {
            accepted();
        }
    });
    let edit = widths.clone();
    let restore = refresh.clone();
    paned.connect_cancel_position(move |_| {
        edit.editing.set(None);
        edit.edit_start.set(None);
        let restore = restore.clone();
        gtk::glib::idle_add_local_once(move || restore());
        false
    });
    let edit = widths.clone();
    paned.connect_position_notify(move |paned| {
        if edit.applying.get() || edit.editing.get() != Some(edge) || paned.width() <= 0 {
            return;
        }
        let other = if edge == SidebarEdge::Navigation {
            paned
                .end_child()
                .and_downcast::<gtk::Paned>()
                .and_then(|inner| inner.end_child())
                .filter(|child| child.is_visible())
                .map_or(0, |child| {
                    child.width() + tokens::SIDEBAR_DIVIDER_WIDTH as i32
                })
        } else {
            0
        };
        let minimum = if edge == SidebarEdge::Navigation {
            tokens::MIN_NAVIGATION_WIDTH
        } else {
            tokens::MIN_INSPECTOR_WIDTH
        } as i32;
        let maximum = if edge == SidebarEdge::Navigation {
            tokens::MAX_NAVIGATION_WIDTH
        } else {
            tokens::MAX_INSPECTOR_WIDTH
        } as i32;
        let available = (paned.width()
            - tokens::SIDEBAR_DIVIDER_WIDTH as i32
            - tokens::MIN_CENTER_WIDTH as i32
            - other)
            .max(minimum);
        let size = if edge == SidebarEdge::Navigation {
            paned.position()
        } else {
            paned.width() - paned.position() - tokens::SIDEBAR_DIVIDER_WIDTH as i32
        };
        let size = size.clamp(minimum, maximum.min(available));
        let position = if edge == SidebarEdge::Navigation {
            size
        } else {
            paned.width() - size - tokens::SIDEBAR_DIVIDER_WIDTH as i32
        };
        if position != paned.position() {
            edit.applying.set(true);
            paned.set_position(position);
            edit.applying.set(false);
        }
    });
    let edit = widths.clone();
    paned.connect_unmap(move |_| {
        if edit.editing.get() == Some(edge) {
            edit.editing.set(None);
            edit.edit_start.set(None);
        }
    });
    let watched = std::rc::Rc::new(std::cell::Cell::new(false));
    let edit = widths.clone();
    paned.connect_map(move |paned| {
        if watched.replace(true) {
            return;
        }
        if let Some(window) = paned.root().and_downcast::<gtk::Window>() {
            let edit = edit.clone();
            let refresh = refresh.clone();
            window.connect_is_active_notify(move |window| {
                if !window.is_active() && edit.editing.get() == Some(edge) {
                    edit.editing.set(None);
                    edit.edit_start.set(None);
                    let refresh = refresh.clone();
                    gtk::glib::idle_add_local_once(move || refresh());
                }
            });
        }
    });
}

/// A narrow inspector is an overlay rather than a Paned end child. Its own
/// six-pixel edge keeps the same preference and never captures canvas input.
fn install_drawer_handle(
    grip: &gtk::Box,
    drawer: &gtk::ScrolledWindow,
    widths: &std::rc::Rc<SidebarWidths>,
    changed: std::rc::Rc<dyn Fn(SidebarEdge, u32)>,
    refresh: std::rc::Rc<dyn Fn()>,
) {
    grip.add_css_class("workspace-drawer-resize-handle");
    grip.set_width_request(tokens::SIDEBAR_DIVIDER_WIDTH as i32);
    grip.set_cursor_from_name(Some("col-resize"));
    grip.set_focusable(true);
    grip.set_focus_on_click(false);
    grip.set_accessible_role(gtk::AccessibleRole::Separator);
    grip.update_property(&[
        gtk::accessible::Property::Label("Resize right sidebar drawer"),
        gtk::accessible::Property::Description("Drag to resize the right sidebar. Left and Right arrows resize when this divider has keyboard focus; Home restores the default width."),
    ]);
    grip.set_tooltip_text(Some(
        "Resize right sidebar · Drag, or focus and use arrow keys",
    ));
    let start_x = std::rc::Rc::new(std::cell::Cell::new(0.0_f64));
    let start_width = std::rc::Rc::new(std::cell::Cell::new(0_u32));
    let drag = gtk::GestureDrag::new();
    drag.set_button(1);
    let preview = widths.clone();
    let drawer_weak = drawer.downgrade();
    let origin = start_x.clone();
    let initial = start_width.clone();
    drag.connect_drag_begin(move |gesture, _, _| {
        if let (Some((x, _)), Some(drawer)) = (
            gesture.current_event().and_then(|event| event.position()),
            drawer_weak.upgrade(),
        ) {
            origin.set(x);
            initial.set(drawer.width().max(0) as u32);
            preview.editing.set(Some(SidebarEdge::Inspector));
            preview.edit_start.set(None);
            preview.drawer_preview.set(None);
        }
    });
    let preview = widths.clone();
    let repaint = refresh.clone();
    let origin = start_x.clone();
    let initial = start_width.clone();
    drag.connect_drag_update(move |gesture, _, _| {
        let Some((x, _)) = gesture.current_event().and_then(|event| event.position()) else {
            return;
        };
        if preview.editing.get() != Some(SidebarEdge::Inspector)
            || preview.edit_start.get().is_some()
        {
            return;
        }
        let delta = origin.get() - x;
        if delta.abs() < 1.0 {
            preview.drawer_preview.set(None);
            repaint();
            return;
        }
        let maximum = gesture
            .widget()
            .and_then(|widget| widget.root())
            .and_downcast::<gtk::Window>()
            .map_or(tokens::MAX_INSPECTOR_WIDTH, |window| {
                (window.width() as f32
                    - tokens::RAIL_WIDTH
                    - tokens::SIDEBAR_DIVIDER_WIDTH
                    - 2.0 * tokens::CONTROL_HEIGHT)
                    .min(tokens::MAX_INSPECTOR_WIDTH)
            })
            .max(tokens::MIN_INSPECTOR_WIDTH);
        let width = (f64::from(initial.get()) + delta)
            .round()
            .clamp(f64::from(tokens::MIN_INSPECTOR_WIDTH), f64::from(maximum))
            as u32;
        preview.drawer_preview.set(Some(width));
        repaint();
    });
    let preview = widths.clone();
    let repaint = refresh.clone();
    let committed = changed.clone();
    drag.connect_drag_end(move |_, _, _| {
        preview.editing.set(None);
        if let Some(width) = preview
            .drawer_preview
            .take()
            .filter(|width| *width != start_width.get())
        {
            preview.inspector.set(width);
            committed(SidebarEdge::Inspector, width);
        }
        repaint();
    });
    let preview = widths.clone();
    let repaint = refresh.clone();
    drag.connect_cancel(move |_, _| {
        preview.editing.set(None);
        preview.drawer_preview.set(None);
        repaint();
    });
    grip.add_controller(drag);
    let keys = gtk::EventControllerKey::new();
    let preferred = widths.clone();
    let keyboard_drawer = drawer.downgrade();
    keys.connect_key_pressed(move |_, key, _, modifiers| {
        let current = keyboard_drawer
            .upgrade()
            .map_or(preferred.inspector.get(), |drawer| {
                drawer.width().max(tokens::MIN_INSPECTOR_WIDTH as i32) as u32
            });
        let step = if modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK) {
            40
        } else {
            10
        };
        let width = match key {
            gtk::gdk::Key::Left => current.saturating_add(step),
            gtk::gdk::Key::Right => current.saturating_sub(step),
            gtk::gdk::Key::Home => tokens::INSPECTOR_WIDTH as u32,
            _ => return gtk::glib::Propagation::Proceed,
        }
        .clamp(
            tokens::MIN_INSPECTOR_WIDTH as u32,
            keyboard_drawer
                .upgrade()
                .and_then(|drawer| drawer.root())
                .and_downcast::<gtk::Window>()
                .map_or(tokens::MAX_INSPECTOR_WIDTH as u32, |window| {
                    (window.width() as f32
                        - tokens::RAIL_WIDTH
                        - tokens::SIDEBAR_DIVIDER_WIDTH
                        - 2.0 * tokens::CONTROL_HEIGHT)
                        .clamp(tokens::MIN_INSPECTOR_WIDTH, tokens::MAX_INSPECTOR_WIDTH)
                        as u32
                }),
        );
        if width == current && key != gtk::gdk::Key::Home {
            return gtk::glib::Propagation::Stop;
        }
        preferred.inspector.set(width);
        changed(SidebarEdge::Inspector, width);
        refresh();
        gtk::glib::Propagation::Stop
    });
    grip.add_controller(keys);
    let preview = widths.clone();
    grip.connect_unmap(move |_| {
        preview.drawer_preview.set(None);
        if preview.edit_start.get().is_none() {
            preview.editing.set(None);
        }
    });
}

/// GTK allocation signals observe size changes without a permanent frame tick.
pub(crate) fn observe_layout(
    host: &gtk::Overlay,
    chrome: &Chrome,
    navigation: &gtk::Widget,
    inspector: &gtk::Box,
    navigation_split: &gtk::Paned,
    inspector_split: &gtk::Paned,
    sender: &ComponentSender<AppModel>,
) {
    let observer = gtk::DrawingArea::new();
    observer.set_can_target(false);
    observer.set_focusable(false);
    observer.set_halign(gtk::Align::Fill);
    observer.set_valign(gtk::Align::Fill);
    host.add_overlay(&observer);
    host.set_measure_overlay(&observer, false);
    // Opt-in acceptance diagnostics sample actual native allocation after the
    // resize settles. They do not feed geometry back into layout or input.
    let geometry = std::env::var_os("ANVIL_BLOCK_REFERENCE_METRICS").map(|_| {
        [
            ("titlebar", chrome.top.upcast_ref::<gtk::Widget>()),
            ("rail", chrome.rail.upcast_ref()),
            ("navigation", navigation),
            ("center", chrome.center.upcast_ref()),
            ("header", chrome.canvas_header.upcast_ref()),
            ("inspector", inspector.upcast_ref()),
            ("block_composer", chrome.block_shelf.upcast_ref()),
            ("search", chrome.search.upcast_ref()),
            (
                "inspector_toggle",
                chrome.inspector_control.button.upcast_ref(),
            ),
        ]
        .into_iter()
        .map(|(name, widget)| (name, widget.downgrade()))
        .collect::<Vec<_>>()
    });
    // A single, non-measuring drawer retains the same inspector on narrow
    // windows. Its viewport is also retained when the panel docks again.
    let drawer = gtk::ScrolledWindow::new();
    drawer.add_css_class("workspace-inspector-drawer");
    drawer.set_policy(gtk::PolicyType::External, gtk::PolicyType::Never);
    drawer.set_min_content_width(tokens::INSPECTOR_WIDTH as i32);
    drawer.set_max_content_width(tokens::INSPECTOR_WIDTH as i32);
    drawer.set_propagate_natural_width(true);
    drawer.set_halign(gtk::Align::End);
    drawer.set_valign(gtk::Align::Fill);
    let drawer_frame = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    drawer_frame.add_css_class("workspace-inspector-drawer-frame");
    drawer_frame.set_halign(gtk::Align::End);
    drawer_frame.set_valign(gtk::Align::Fill);
    drawer_frame.set_visible(false);
    let drawer_grip = gtk::Box::new(gtk::Orientation::Vertical, 0);
    drawer_frame.append(&drawer_grip);
    drawer_frame.append(&drawer);
    let drawer_viewport = gtk::Viewport::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
    drawer_viewport.set_scroll_to_focus(false);
    drawer.set_child(Some(&drawer_viewport));
    host.add_overlay(&drawer_frame);
    host.set_measure_overlay(&drawer_frame, false);
    let native_navigation_split = navigation_split.clone();
    let native_inspector_split = inspector_split.clone();
    let native_drawer = drawer.clone();
    let navigation = navigation.downgrade();
    let inspector = inspector.downgrade();
    let navigation_split = navigation_split.downgrade();
    let inspector_split = inspector_split.downgrade();
    let drawer = drawer.downgrade();
    let drawer_frame = drawer_frame.downgrade();
    let drawer_viewport = drawer_viewport.downgrade();
    let observer_weak = observer.downgrade();
    let control = std::rc::Rc::downgrade(&chrome.inspector_control);
    let widths = chrome.sidebar_widths.clone();
    let diagnostic_epoch = std::rc::Rc::new(std::cell::Cell::new(0_u64));
    let apply: std::rc::Rc<dyn Fn()> = std::rc::Rc::new(move || {
        let (
            Some(observer),
            Some(control),
            Some(inspector),
            Some(split),
            Some(drawer),
            Some(drawer_frame),
            Some(drawer_viewport),
        ) = (
            observer_weak.upgrade(),
            control.upgrade(),
            inspector.upgrade(),
            inspector_split.upgrade(),
            drawer.upgrade(),
            drawer_frame.upgrade(),
            drawer_viewport.upgrade(),
        )
        else {
            return;
        };
        let width = observer.width();
        let height = observer.height();
        if width <= 0 || height <= 0 {
            return;
        }
        let base = tokens::Layout::for_size_with_sidebars(
            width as f32,
            height as f32 + tokens::TITLEBAR_HEIGHT,
            if widths.navigation_visible.get() {
                widths.navigation.get() as f32
            } else {
                0.0
            },
            if control.requested.get() == Some(false) {
                0.0
            } else {
                widths.inspector.get() as f32
            },
        );
        let panel_focus = observer
            .root()
            .and_downcast::<gtk::Window>()
            .and_then(|window| gtk::prelude::RootExt::focus(&window))
            .filter(|focus| crate::widget_is_within(focus.clone(), inspector.upcast_ref()));
        if panel_focus.is_some() && base.inspector.is_none() && control.requested.get().is_none() {
            // Keep a focused panel in a drawer rather than hiding its owner
            // merely because the default presentation auto-collapses.
            control.requested.set(Some(true));
        }
        let drawer_width = (width as f32
            - tokens::RAIL_WIDTH
            - tokens::SIDEBAR_DIVIDER_WIDTH
            - 2.0 * tokens::CONTROL_HEIGHT)
            .max(0.0)
            .min(
                widths
                    .drawer_preview
                    .get()
                    .unwrap_or_else(|| widths.inspector.get()) as f32,
            );
        let drawer_available = drawer_width >= tokens::MIN_INSPECTOR_WIDTH
            && height as f32 >= tokens::INSPECTOR_TABS_HEIGHT + tokens::CONTROL_HEIGHT;
        let placement = if control.requested.get() == Some(false) {
            InspectorPlacement::Hidden
        } else if base.inspector.is_some() {
            InspectorPlacement::Docked
        } else if control.requested.get() == Some(true) && drawer_available {
            InspectorPlacement::Drawer
        } else {
            InspectorPlacement::Hidden
        };
        let available = base.inspector.is_some() || drawer_available;
        drawer.set_max_content_width(-1);
        drawer.set_min_content_width(drawer_width as i32);
        drawer.set_max_content_width(drawer_width as i32);
        drawer.set_width_request(drawer_width as i32);
        drawer_frame.set_width_request(drawer_width as i32 + tokens::SIDEBAR_DIVIDER_WIDTH as i32);
        let layout = if placement == InspectorPlacement::Docked {
            base
        } else {
            base.without_inspector()
        };
        let in_drawer =
            drawer_viewport.child().as_ref() == Some(inspector.upcast_ref::<gtk::Widget>());
        let moving = (placement == InspectorPlacement::Drawer && !in_drawer)
            || (placement == InspectorPlacement::Docked && in_drawer);
        match placement {
            InspectorPlacement::Drawer if !in_drawer => {
                split.set_end_child(None::<&gtk::Widget>);
                drawer_viewport.set_child(Some(&inspector));
            }
            InspectorPlacement::Docked if in_drawer => {
                drawer_viewport.set_child(None::<&gtk::Widget>);
                split.set_end_child(Some(&inspector));
            }
            _ => {}
        }
        inspector.set_visible(placement != InspectorPlacement::Hidden);
        drawer_frame.set_visible(placement == InspectorPlacement::Drawer);
        if moving {
            if let Some(focus) = panel_focus.filter(|focus| {
                focus.is_mapped() && focus.root().as_ref() == observer.root().as_ref()
            }) {
                // GTK repairs focus synchronously to a container while the
                // original input is temporarily unparented. Finish this
                // synchronous remap before arming the existing idle guard;
                // actual later user choices still supersede that guard.
                focus.grab_focus();
                crate::block_view::restore_live_focus_deferred(&focus);
            }
        }
        if let Some(navigation) = navigation.upgrade() {
            navigation.set_visible(layout.navigation.is_some());
        }
        if let Some(navigation) = navigation_split.upgrade() {
            apply_sidebar_positions(&navigation, &split, &layout, &widths);
        }
        control.sync_button(placement, available);
        if let Some(nodes) = &geometry {
            let epoch = diagnostic_epoch.get().wrapping_add(1);
            diagnostic_epoch.set(epoch);
            schedule_geometry_sample(&observer, nodes.clone(), epoch, diagnostic_epoch.clone());
        }
    });
    *chrome.inspector_control.refresh.borrow_mut() = Some(apply.clone());
    let width_sender = sender.clone();
    let changed: std::rc::Rc<dyn Fn(SidebarEdge, u32)> = std::rc::Rc::new(move |edge, width| {
        width_sender.input(match edge {
            SidebarEdge::Navigation => AppMsg::NavigationWidthChanged(width),
            SidebarEdge::Inspector => AppMsg::AiPanelWidthChanged(width),
        });
    });
    install_sidebar_handle(
        &native_navigation_split,
        SidebarEdge::Navigation,
        &chrome.sidebar_widths,
        changed.clone(),
        apply.clone(),
    );
    install_sidebar_handle(
        &native_inspector_split,
        SidebarEdge::Inspector,
        &chrome.sidebar_widths,
        changed.clone(),
        apply.clone(),
    );
    install_drawer_handle(
        &drawer_grip,
        &native_drawer,
        &chrome.sidebar_widths,
        changed,
        apply.clone(),
    );
    observe_sidebar_allocations(
        &[&native_navigation_split, &native_inspector_split],
        apply.clone(),
    );
    observer.connect_resize(move |_, _, _| apply());
}

// This opt-in observer never drives layout or input. Idle after a resize can
// precede the descendants' next allocation; sample actual after-paint bounds
// instead, bounded to eight frames and requiring two equal valid samples.
fn schedule_geometry_sample(
    observer: &gtk::DrawingArea,
    nodes: Vec<(&'static str, gtk::glib::WeakRef<gtk::Widget>)>,
    epoch: u64,
    current_epoch: std::rc::Rc<std::cell::Cell<u64>>,
) {
    let observer = observer.downgrade();
    gtk::glib::idle_add_local_once(move || {
        let Some(window) = observer
            .upgrade()
            .and_then(|widget| widget.root())
            .and_downcast::<gtk::Window>()
        else {
            return;
        };
        let Some(clock) = window.frame_clock() else {
            return;
        };
        let window = window.downgrade();
        let frames = std::cell::Cell::new(0_u8);
        let stable = std::cell::Cell::new(0_u8);
        let last = std::cell::RefCell::new(None::<serde_json::Value>);
        let handler = std::rc::Rc::new(std::cell::RefCell::new(None));
        let stored_handler = handler.clone();
        let id = clock.connect_after_paint(move |clock| {
            let stale = current_epoch.get() != epoch;
            let sample = if !stale { window.upgrade().map(|window| collect_native_geometry(&window, &nodes)) } else { None };
            let frame = frames.get().saturating_add(1);
            frames.set(frame);
            let mut done = stale || sample.is_none();
            if let Some(mut sample) = sample {
                let window_size = sample["window"].as_array().expect("geometry window");
                let width = window_size[0].as_f64().unwrap_or(0.0);
                let height = window_size[1].as_f64().unwrap_or(0.0);
                let regions = sample["regions"].as_object().expect("geometry regions");
                let within = |rect: &serde_json::Value| {
                    let x=rect["x"].as_f64().unwrap_or(-1.0); let y=rect["y"].as_f64().unwrap_or(-1.0);
                    let w=rect["width"].as_f64().unwrap_or(0.0); let h=rect["height"].as_f64().unwrap_or(0.0);
                    x>=0.0 && y>=0.0 && w>0.0 && h>0.0 && x+w<=width+0.5 && y+h<=height+0.5
                };
                let composer = regions.get("chat_composer").or_else(|| regions.get("block_composer"));
                let valid = composer.is_some_and(within) && regions.get("footer").is_some_and(within);
                // Streaming history changes its adjustment independently of
                // shell allocation. Report it, but never wait for output to
                // stop before accepting two equal structural samples.
                let mut structural_sample = sample.clone();
                structural_sample.as_object_mut().unwrap().remove("canvas_scroll");
                let equal = last.borrow().as_ref() == Some(&structural_sample);
                stable.set(if valid { if equal { stable.get().saturating_add(1) } else { 1 } } else { 0 });
                *last.borrow_mut() = Some(structural_sample);
                sample["sample_phase"] = serde_json::json!("after-paint");
                sample["sample_epoch"] = serde_json::json!(epoch);
                sample["sample_frame"] = serde_json::json!(frame);
                sample["stable_frames"] = serde_json::json!(stable.get());
                sample["bounds_within_window"] = serde_json::json!(valid);
                if stable.get() >= 2 {
                    log::info!(target:"anvil::workspace_shell","native_shell_geometry {sample}");
                    done = true;
                } else {
                    log::info!(target:"anvil::workspace_shell","native_shell_geometry_observation {sample}");
                }
            }
            if frame >= 8 { done=true; }
            if done {
                let id = stored_handler.borrow_mut().take();
                if let Some(id) = id { clock.disconnect(id); }
                clock.end_updating();
            }
        });
        *handler.borrow_mut() = Some(id);
        // Diagnostic-only clock request. This is not a claim about ordinary
        // startup frame continuity, native consumption, or prompt readiness.
        clock.begin_updating();
    });
}

fn collect_native_geometry(
    window: &gtk::Window,
    nodes: &[(&'static str, gtk::glib::WeakRef<gtk::Widget>)],
) -> serde_json::Value {
    fn retained_child(widget: &gtk::Widget, class: &str) -> Option<gtk::Widget> {
        if widget.has_css_class(class) {
            return Some(widget.clone());
        }
        let mut child = widget.first_child();
        while let Some(node) = child {
            if let Some(found) = retained_child(&node, class) {
                return Some(found);
            }
            child = node.next_sibling();
        }
        None
    }
    fn mapped_child(widget: &gtk::Widget, class: &str) -> Option<gtk::Widget> {
        if !widget.is_mapped() {
            return None;
        }
        if widget.has_css_class(class) {
            return Some(widget.clone());
        }
        let mut child = widget.first_child();
        while let Some(node) = child {
            if let Some(found) = mapped_child(&node, class) {
                return Some(found);
            }
            child = node.next_sibling();
        }
        None
    }
    let bounds = |widget: &gtk::Widget| {
        widget.compute_bounds(window).map(|rect| {
            serde_json::json!({"x":rect.x(),"y":rect.y(),"width":rect.width(),"height":rect.height()})
        })
    };
    let mut regions = serde_json::Map::new();
    for (name, widget) in nodes {
        if let Some(widget) = widget.upgrade().filter(|widget| widget.is_mapped()) {
            if let Some(rect) = bounds(&widget) {
                regions.insert(name.to_string(), rect);
            }
        }
    }
    for (name, class) in [
        ("chat_composer", "chat-composer"),
        ("header_title", "shell-canvas-title"),
        ("live_toolbar", "block-reference-toolbar"),
        ("footer", "reference-composer-controls"),
        ("footer_model", "shell-composer-model"),
        ("footer_send", "chat-send"),
        ("footer_stop", "chat-stop"),
        ("footer_tools", "reference-tools-button"),
        ("inspector_drawer", "workspace-inspector-drawer"),
        ("inspector_live_host", "shell-inspector-live-scroll"),
        ("inspector_overview", "reference-rich-inspector"),
        ("inspector_scroll", "reference-inspector-scroll"),
        (
            "inspector_agent_heading",
            "reference-inspector-agent-heading",
        ),
        ("inspector_metadata", "reference-inspector-metadata"),
        ("inspector_current", "reference-inspector-current"),
        ("inspector_queue", "reference-inspector-queue"),
        (
            "inspector_terminal_section",
            "reference-inspector-terminal-section",
        ),
        (
            "inspector_output_heading",
            "reference-inspector-output-heading",
        ),
        ("inspector_output", "reference-inspector-output"),
    ] {
        if let Some(widget) = mapped_child(window.upcast_ref(), class) {
            if let Some(rect) = bounds(&widget) {
                regions.insert(name.to_string(), rect);
            }
        }
    }
    fn footer_buttons(
        widget: &gtk::Widget,
        window: &gtk::Window,
        output: &mut Vec<serde_json::Value>,
    ) {
        if !widget.is_mapped() {
            return;
        }
        if widget.is::<gtk::Button>() || widget.is::<gtk::MenuButton>() {
            if let Some(rect) = widget.compute_bounds(window) {
                output.push(serde_json::json!({
                    "classes":widget.css_classes().iter().map(ToString::to_string).collect::<Vec<_>>(),
                    "bounds":[rect.x(),rect.y(),rect.width(),rect.height()],
                    "sensitive":widget.is_sensitive(),
                }));
            }
        }
        let mut child = widget.first_child();
        while let Some(node) = child {
            footer_buttons(&node, window, output);
            child = node.next_sibling();
        }
    }
    let mut buttons = Vec::new();
    if let Some(footer) = mapped_child(window.upcast_ref(), "reference-composer-controls") {
        footer_buttons(&footer, window, &mut buttons);
    }
    let badge_style = mapped_child(window.upcast_ref(), "reference-neutral-badge").map(|badge| {
        #[allow(deprecated)]
        let color = badge.style_context().color();
        serde_json::json!({
            "bounds":bounds(&badge),
            "foreground_rgba":[color.red(),color.green(),color.blue(),color.alpha()],
            "font":badge.pango_context().font_description().map(|font| font.to_string()),
        })
    });
    fn minimum_width_chain(widget: &gtk::Widget, path: &str, output: &mut Vec<serde_json::Value>) {
        let (minimum, natural, _, _) = widget.measure(gtk::Orientation::Horizontal, -1);
        if minimum >= 280 {
            let stack = widget.downcast_ref::<gtk::Stack>();
            output.push(serde_json::json!({
                "path":path,"type":widget.type_().name(),
                "classes":widget.css_classes().iter().map(|class|class.as_str()).collect::<Vec<_>>(),
                "minimum":minimum,"natural":natural,"width_request":widget.width_request(),
                "visible":widget.is_visible(),"mapped":widget.is_mapped(),
                "stack_hhomogeneous":stack.map(|stack|stack.is_hhomogeneous()),
                "stack_selected":stack.and_then(|stack|stack.visible_child_name()).map(|name|name.to_string()),
                "label_chars":widget.downcast_ref::<gtk::Label>().map(|label|label.text().chars().count()),
            }));
        }
        let mut index = 0;
        let mut child = widget.first_child();
        while let Some(node) = child {
            minimum_width_chain(&node, &format!("{path}/{index}"), output);
            index += 1;
            child = node.next_sibling();
        }
    }
    let right_sidebar = retained_child(window.upcast_ref(), "workspace-inspector").map(|panel| {
        let mut minimum_widths = Vec::new();
        minimum_width_chain(&panel, "inspector", &mut minimum_widths);
        let pages = retained_child(&panel, "shell-inspector-pages")
            .and_then(|pages| pages.downcast::<gtk::Stack>().ok());
        let live_host = retained_child(&panel, "shell-inspector-live-scroll")
            .and_then(|host|host.downcast::<gtk::ScrolledWindow>().ok());
        let live_pages = retained_child(&panel, "shell-inspector-live-pages")
            .and_then(|pages| pages.downcast::<gtk::Stack>().ok());
        let toggle = mapped_child(window.upcast_ref(), "shell-inspector-toggle")
            .and_then(|toggle| toggle.downcast::<gtk::ToggleButton>().ok());
        let focus = gtk::prelude::RootExt::focus(window);
        serde_json::json!({
            "visible":panel.is_visible(),
            "mapped":panel.is_mapped(),
            "minimum_width_chain":minimum_widths,
            "live_scroll":live_host.as_ref().map(|host| {
                let horizontal = host.hadjustment();
                let vertical = host.vadjustment();
                serde_json::json!({
                    "object":format!("{:p}",host.as_ptr()),"mapped":host.is_mapped(),
                    "horizontal":[horizontal.value(),horizontal.upper(),horizontal.page_size()],
                    "vertical":[vertical.value(),vertical.upper(),vertical.page_size()],
                    "horizontal_scrollbar_mapped":host.hscrollbar().is_mapped(),
                    "vertical_scrollbar_mapped":host.vscrollbar().is_mapped(),
                })
            }),
            "panel_object":format!("{:p}",panel.as_ptr()),
            "pages_object":pages.as_ref().map(|pages|format!("{:p}",pages.as_ptr())),
            "selected_page":pages.as_ref().and_then(|pages|pages.visible_child_name()).map(|name|name.to_string()),
            "selected_live_page":live_pages.as_ref().and_then(|pages|pages.visible_child_name()).map(|name|name.to_string()),
            "drawer":panel.is_mapped() && panel.ancestor(gtk::ScrolledWindow::static_type())
                .is_some_and(|parent|parent.has_css_class("workspace-inspector-drawer")),
            "toggle_active":toggle.as_ref().is_some_and(gtk::prelude::ToggleButtonExt::is_active),
            "toggle_sensitive":toggle.as_ref().is_some_and(gtk::prelude::WidgetExt::is_sensitive),
            "tooltip":toggle.as_ref().and_then(gtk::prelude::WidgetExt::tooltip_text).map(|text|text.to_string()),
            "focus_object":focus.as_ref().map(|focus|format!("{:p}",focus.as_ptr())),
            "focus_type":focus.as_ref().map(|focus|focus.type_().name()),
            "focus_in_panel":focus.is_some_and(|focus|crate::widget_is_within(focus,&panel)),
        })
    });
    let canvas_scroll = mapped_child(window.upcast_ref(), "block-scroll")
        .and_then(|scroll| scroll.downcast::<gtk::ScrolledWindow>().ok())
        .map(|scroll| {
            let adjustment = scroll.vadjustment();
            serde_json::json!({
                "object":format!("{:p}",scroll.as_ptr()),
                "value":adjustment.value(),"upper":adjustment.upper(),
                "page_size":adjustment.page_size(),
            })
        });
    serde_json::json!({"window":[window.width(),window.height()],"dpr":window.scale_factor(),"regions":regions,"footer_buttons":buttons,"badge_style":badge_style,"window_opacity":window.opacity(),"right_sidebar":right_sidebar,"canvas_scroll":canvas_scroll})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an isolated GTK display; real popup paint and live mode changes"]
    fn workspace_popups_follow_launcher_palette_without_composer_style_leaks() {
        use gtk::gdk::prelude::PaintableExt;

        gtk::init().expect("GTK display");
        install_css();
        let settle = || {
            let until = std::time::Instant::now() + std::time::Duration::from_millis(200);
            while std::time::Instant::now() < until {
                while gtk::glib::MainContext::default().iteration(false) {}
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        };
        let assert_color = |actual: gtk::gdk::RGBA, expected: &str| {
            let expected = gtk::gdk::RGBA::parse(expected).unwrap();
            for (actual, expected) in [
                (actual.red(), expected.red()),
                (actual.green(), expected.green()),
                (actual.blue(), expected.blue()),
            ] {
                assert!((actual - expected).abs() < 0.01, "{actual} != {expected}");
            }
        };
        for tools in [false, true] {
            let window = gtk::Window::new();
            window.add_css_class("anvil-font-sync");
            window.set_default_size(640, 480);
            let launcher_surface = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let footer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            footer.add_css_class("chat-controls");
            let anchor = gtk::MenuButton::new();
            anchor.set_label("Menu");
            footer.append(&anchor);
            launcher_surface.append(&footer);
            window.set_child(Some(&launcher_surface));
            let popover = gtk::Popover::new();
            popover.set_has_arrow(false);
            if tools {
                popover.add_css_class("reference-tools-popover");
                popover.set_widget_name("reference-tools-menu");
            }
            let entries = gtk::Box::new(gtk::Orientation::Vertical, 4);
            entries.set_width_request(280);
            let item = gtk::Button::with_label("Run command");
            let disabled = gtk::Button::with_label("Unavailable command");
            disabled.set_sensitive(false);
            if tools {
                item.add_css_class("reference-tool-row");
                disabled.add_css_class("reference-tool-row");
            }
            entries.append(&item);
            entries.append(&disabled);
            popover.set_child(Some(&entries));
            anchor.set_popover(Some(&popover));
            window.present();
            anchor.popup();
            for (mode, background, foreground, muted) in [
                ("sidebar", "#162133", "#e1e9f6", "#91a2bb"),
                ("chat", "#fefefe", "#35445e", "#728099"),
                ("block", "#1c293a", "#d5deec", "#91a4bf"),
                ("chat", "#fefefe", "#35445e", "#728099"),
            ] {
                for class in [
                    "workspace-navigation",
                    "workspace-center",
                    "block-workspace",
                ] {
                    launcher_surface.remove_css_class(class);
                }
                if mode == "sidebar" {
                    launcher_surface.add_css_class("workspace-navigation");
                } else {
                    launcher_surface.add_css_class("workspace-center");
                    if mode == "block" {
                        launcher_surface.add_css_class("block-workspace");
                    }
                }
                settle();
                assert!(popover.is_mapped(), "mode changes keep the menu open");
                assert_color(item.child().unwrap().color(), foreground);
                assert_color(disabled.child().unwrap().color(), muted);
                let snapshot = gtk::Snapshot::new();
                gtk::WidgetPaintable::new(Some(&popover)).snapshot(
                    &snapshot,
                    f64::from(popover.width()),
                    f64::from(popover.height()),
                );
                let node = snapshot.to_node().expect("mapped menu paint");
                let renderer = gtk::gsk::Renderer::for_surface(&window.surface().unwrap()).unwrap();
                let texture = renderer.render_texture(&node, None);
                let stride = texture.width() as usize * 4;
                let mut pixels = vec![0; stride * texture.height() as usize];
                texture.download(&mut pixels, stride);
                let color = gtk::gdk::RGBA::parse(background).unwrap();
                let expected = [
                    (color.red() * 255.0).round() as u8,
                    (color.green() * 255.0).round() as u8,
                    (color.blue() * 255.0).round() as u8,
                ];
                let matches = pixels
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .filter(|pixel| {
                        let argb = u32::from_ne_bytes([pixel[0], pixel[1], pixel[2], pixel[3]]);
                        [
                            ((argb >> 16) & 255) as u8,
                            ((argb >> 8) & 255) as u8,
                            (argb & 255) as u8,
                        ] == expected
                    })
                    .count();
                assert!(
                    matches > 100,
                    "{mode}, tools={tools}: actual popup background"
                );
                renderer.unrealize();
            }
            popover.popdown();
            window.close();
            settle();
        }
    }

    #[test]
    #[ignore = "requires an isolated GTK display; actual adaptive dialog and dimmed underlay"]
    fn workspace_dialog_themes_its_sheet_without_painting_over_the_workspace() {
        use relm4::adw;
        use relm4::adw::prelude::*;

        adw::init().expect("GTK display");
        struct AnimationPreference(gtk::Settings, bool);
        impl Drop for AnimationPreference {
            fn drop(&mut self) {
                self.0.set_gtk_enable_animations(self.1);
            }
        }
        let settings = gtk::Settings::default().unwrap();
        let _animations =
            AnimationPreference(settings.clone(), settings.is_gtk_enable_animations());
        settings.set_gtk_enable_animations(false);
        install_css();
        let display = gtk::gdk::Display::default().unwrap();
        let underlay_css = gtk::CssProvider::new();
        underlay_css.load_from_string(".popup-test-underlay { background:#49a78a; }");
        gtk::style_context_add_provider_for_display(
            &display,
            &underlay_css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 21,
        );
        let window = adw::Window::new();
        window.set_decorated(false);
        window.set_default_size(800, 600);
        let underlay = gtk::Box::new(gtk::Orientation::Vertical, 0);
        underlay.add_css_class("popup-test-underlay");
        window.set_content(Some(&underlay));
        window.present();
        let settle = || {
            let until = std::time::Instant::now() + std::time::Duration::from_millis(250);
            while std::time::Instant::now() < until {
                while gtk::glib::MainContext::default().iteration(false) {}
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        };
        let pixel = |x: usize, y: usize| {
            let snapshot = gtk::Snapshot::new();
            gtk::WidgetPaintable::new(Some(&window)).snapshot(
                &snapshot,
                f64::from(window.width()),
                f64::from(window.height()),
            );
            let node = snapshot.to_node().expect("mapped native dialog host");
            let renderer = gtk::gsk::Renderer::for_surface(&window.surface().unwrap()).unwrap();
            let texture = renderer.render_texture(&node, None);
            let stride = texture.width() as usize * 4;
            let mut pixels = vec![0; stride * texture.height() as usize];
            texture.download(&mut pixels, stride);
            let offset = y * stride + x * 4;
            let argb = u32::from_ne_bytes(pixels[offset..offset + 4].try_into().unwrap());
            renderer.unrealize();
            [
                ((argb >> 16) & 255) as u8,
                ((argb >> 8) & 255) as u8,
                (argb & 255) as u8,
            ]
        };
        settle();
        assert_eq!(
            pixel(20, 20),
            [0x49, 0xa7, 0x8a],
            "distinct workspace underlay"
        );
        let dialog = adw::Dialog::new();
        dialog.add_css_class("workspace-dialog");
        dialog.set_content_width(320);
        dialog.set_content_height(240);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&gtk::Box::new(gtk::Orientation::Vertical, 0)));
        dialog.set_child(Some(&toolbar));
        dialog.present(Some(&window));
        settle();
        assert!(dialog.is_mapped(), "actual AdwDialog presentation");
        let corner = pixel(20, 20);
        assert!(
            u16::from(corner[1]) > u16::from(corner[0]) + 20
                && u16::from(corner[1]) > u16::from(corner[2]) + 10,
            "the modal corner retains the green workspace through Adwaita dimming: {corner:?}"
        );
        let sheet = pixel(
            window.width() as usize / 2,
            window.height() as usize / 2 + 60,
        );
        assert_eq!(
            sheet,
            [0x16, 0x21, 0x33],
            "the actual dialog body uses workspace navy"
        );
        dialog.close();
        window.close();
        settle();
        gtk::style_context_remove_provider_for_display(&display, &underlay_css);
    }

    #[test]
    #[ignore = "requires isolated X11 DISPLAY and xdotool; actual sidebar drags and Block PTY"]
    fn native_sidebar_drags_keep_preferences_and_terminal_draft_through_responsive_clamps() {
        crate::child_env::capture_inherited_environment().expect("fixture child environment");
        gtk::init().expect("GTK display");
        install_css();
        let view =
            crate::block_view::organism_settings_test_view_for_mode(TerminalMode::Block, None);
        let recorder = crate::block_view::organism_settings_test_recorder(&view);
        let window = gtk::Window::new();
        window.set_title(Some("anvil-sidebar-resize-native-test"));
        // A custom titlebar exercises the same native CSD coordinate transform
        // as the application, rather than a shadowless test-only surface.
        window.set_titlebar(Some(&gtk::HeaderBar::new()));
        window.set_default_size(1600, 650);
        let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        body.add_css_class("workspace-body");
        let rail = gtk::Box::new(gtk::Orientation::Vertical, 0);
        rail.set_width_request(tokens::RAIL_WIDTH as i32);
        body.append(&rail);
        let navigation = gtk::Box::new(gtk::Orientation::Vertical, 0);
        navigation.set_width_request(tokens::MIN_NAVIGATION_WIDTH as i32);
        let inspector = gtk::Box::new(gtk::Orientation::Vertical, 0);
        inspector.set_width_request(tokens::MIN_INSPECTOR_WIDTH as i32);
        let right = gtk::Paned::new(gtk::Orientation::Horizontal);
        right.set_start_child(Some(&view.widget()));
        right.set_end_child(Some(&inspector));
        right.set_resize_start_child(true);
        right.set_resize_end_child(false);
        right.set_shrink_start_child(true);
        right.set_shrink_end_child(false);
        let left = gtk::Paned::new(gtk::Orientation::Horizontal);
        left.set_start_child(Some(&navigation));
        left.set_end_child(Some(&right));
        left.set_resize_start_child(false);
        left.set_resize_end_child(true);
        left.set_shrink_start_child(false);
        left.set_shrink_end_child(true);
        left.set_hexpand(true);
        body.append(&left);
        let host = gtk::Overlay::new();
        host.set_child(Some(&body));
        let observer = gtk::DrawingArea::new();
        observer.set_can_target(false);
        observer.set_halign(gtk::Align::Fill);
        observer.set_valign(gtk::Align::Fill);
        host.add_overlay(&observer);
        host.set_measure_overlay(&observer, false);
        window.set_child(Some(&host));
        let preferred = std::rc::Rc::new(SidebarWidths::new(237, 306, true));
        let right_visible = std::rc::Rc::new(std::cell::Cell::new(true));
        let refresh: std::rc::Rc<dyn Fn()> = {
            let window = window.downgrade();
            let left = left.downgrade();
            let right = right.downgrade();
            let preferred = preferred.clone();
            let right_visible = right_visible.clone();
            std::rc::Rc::new(move || {
                let (Some(window), Some(left), Some(right)) =
                    (window.upgrade(), left.upgrade(), right.upgrade())
                else {
                    return;
                };
                let layout = tokens::Layout::for_size_with_sidebars(
                    window.width() as f32,
                    window.height() as f32,
                    preferred.navigation.get() as f32,
                    if right_visible.get() {
                        preferred.inspector.get() as f32
                    } else {
                        0.0
                    },
                );
                left.start_child()
                    .unwrap()
                    .set_visible(layout.navigation.is_some());
                right
                    .end_child()
                    .unwrap()
                    .set_visible(layout.inspector.is_some());
                apply_sidebar_positions(&left, &right, &layout, &preferred);
            })
        };
        let commits = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let record = commits.clone();
        let changed: std::rc::Rc<dyn Fn(SidebarEdge, u32)> =
            std::rc::Rc::new(move |edge, width| record.borrow_mut().push((edge, width)));
        install_sidebar_handle(
            &left,
            SidebarEdge::Navigation,
            &preferred,
            changed.clone(),
            refresh.clone(),
        );
        install_sidebar_handle(
            &right,
            SidebarEdge::Inspector,
            &preferred,
            changed,
            refresh.clone(),
        );
        observe_sidebar_allocations(&[&left, &right], refresh.clone());
        let reflow = refresh.clone();
        observer.connect_resize(move |_, _, _| reflow());
        let settle = || {
            let until = std::time::Instant::now() + std::time::Duration::from_millis(250);
            while std::time::Instant::now() < until {
                while gtk::glib::MainContext::default().iteration(false) {}
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        };
        let xdo = |arguments: &[String]| {
            let output = std::process::Command::new("xdotool")
                .args(arguments)
                .output()
                .expect("native X11 input helper");
            assert!(
                output.status.success(),
                "xdotool: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap()
        };
        window.present();
        settle();
        refresh();
        settle();
        let xid = xdo(&[
            "search".into(),
            "--onlyvisible".into(),
            "--name".into(),
            "^anvil-sidebar-resize-native-test$".into(),
        ])
        .lines()
        .last()
        .unwrap()
        .to_owned();
        xdo(&["windowfocus".into(), "--sync".into(), xid.clone()]);
        settle();
        recorder.emit_test_output(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        settle();
        view.vte().grab_focus();
        let draft = "printf KEEP_DRAFT";
        view.vte()
            .emit_by_name::<()>("commit", &[&draft, &(draft.len() as u32)]);
        assert_eq!(
            recorder.drain_test_slave(std::time::Duration::from_millis(80)),
            b"printf KEEP_DRAFT"
        );
        recorder.emit_test_output(b"printf KEEP_DRAFT");
        settle();
        let original = view.vte().clone();
        let fd = recorder.master_fd_raw();
        let drag = |paned: &gtk::Paned, delta: i32| {
            let mut child = paned.first_child();
            let handle = loop {
                let widget = child.expect("native sidebar separator");
                if widget.css_name() == "separator" {
                    break widget;
                }
                child = widget.next_sibling();
            };
            let bounds = handle.compute_bounds(&window).unwrap();
            assert!(
                bounds.width() >= tokens::SIDEBAR_DIVIDER_WIDTH - 0.5,
                "real divider hit target"
            );
            let (surface_x, surface_y) = window.surface_transform();
            let x = (f64::from(bounds.x() + bounds.width() / 2.0) + surface_x).round() as i32;
            let y = (f64::from(bounds.y() + bounds.height() / 2.0) + surface_y).round() as i32;
            xdo(&[
                "mousemove".into(),
                "--window".into(),
                xid.clone(),
                x.to_string(),
                y.to_string(),
                "mousedown".into(),
                "1".into(),
            ]);
            settle();
            if delta != 0 {
                xdo(&[
                    "mousemove".into(),
                    "--window".into(),
                    xid.clone(),
                    (x + delta).to_string(),
                    y.to_string(),
                ]);
                settle();
            }
            xdo(&["mouseup".into(), "1".into()]);
            settle();
        };
        drag(&left, 90);
        assert!(
            preferred.navigation.get() > 300,
            "left pointer drag commits a wider sidebar"
        );
        drag(&right, -60);
        assert!(
            preferred.inspector.get() > 350,
            "right pointer drag commits a wider sidebar"
        );
        let saved = (preferred.navigation.get(), preferred.inspector.get());
        assert!(
            view.vte().has_focus(),
            "pointer divider drag does not steal terminal/IME focus"
        );
        assert!(commits
            .borrow()
            .iter()
            .any(|(edge, _)| *edge == SidebarEdge::Navigation));
        assert!(commits
            .borrow()
            .iter()
            .any(|(edge, _)| *edge == SidebarEdge::Inspector));
        right_visible.set(false);
        refresh();
        settle();
        right_visible.set(true);
        refresh();
        settle();
        assert_eq!(
            preferred.inspector.get(),
            saved.1,
            "hide/show keeps requested right width"
        );
        window.set_default_size(760, 650);
        settle();
        assert!(
            left.position() < saved.0 as i32,
            "narrow allocation temporarily clamps navigation"
        );
        let count = commits.borrow().len();
        drag(&left, 0);
        assert_eq!(
            commits.borrow().len(),
            count,
            "a no-motion click on a clamp is not a new preference"
        );
        assert_eq!(
            (preferred.navigation.get(), preferred.inspector.get()),
            saved
        );
        window.set_default_size(1600, 650);
        settle();
        assert_eq!(
            left.position(),
            saved.0 as i32,
            "left preferred width returns after grow"
        );
        assert_eq!(
            inspector.width(),
            saved.1 as i32,
            "right preferred width returns after grow"
        );
        assert_eq!(view.vte(), &original);
        assert_eq!(recorder.master_fd_raw(), fd);
        assert!(
            view.vte()
                .text_format(vte4::Format::Text)
                .unwrap()
                .contains("KEEP_DRAFT"),
            "native draft remains displayed"
        );
        assert!(
            recorder
                .drain_test_slave(std::time::Duration::from_millis(80))
                .is_empty(),
            "resizing does not submit or retype the shell draft"
        );
        window.close();
        recorder.kill();
    }

    #[test]
    fn inspector_toggle_uses_narrow_drawer_and_keeps_the_explicit_choice() {
        for (width, height, choice, placement, available) in [
            (1356.0, 704.0, None, InspectorPlacement::Docked, true),
            (1356.0, 704.0, Some(false), InspectorPlacement::Hidden, true),
            (1356.0, 704.0, Some(true), InspectorPlacement::Docked, true),
            (800.0, 640.0, None, InspectorPlacement::Hidden, true),
            (800.0, 640.0, Some(true), InspectorPlacement::Drawer, true),
            (700.0, 480.0, Some(true), InspectorPlacement::Drawer, true),
            (700.0, 480.0, Some(false), InspectorPlacement::Hidden, true),
            (350.0, 480.0, Some(true), InspectorPlacement::Hidden, false),
            (700.0, 90.0, Some(true), InspectorPlacement::Hidden, false),
        ] {
            assert_eq!(
                inspector_placement(&tokens::Layout::for_size(width, height), choice),
                (placement, available),
                "{width}x{height}, requested={choice:?}"
            );
        }
        let closed = tokens::Layout::for_size(1356.0, 704.0).without_inspector();
        assert!(closed.inspector.is_none());
        assert_eq!(closed.center.width, 1064.0);
        assert_eq!(closed.header.width, 1064.0);
        assert_eq!(closed.composer.width, 1008.0);
        assert_eq!(closed.composer.y, 613.0);
    }

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
        assert_eq!(nominal.size(), 12 * gtk::pango::SCALE);
        assert!(
            nominal.is_size_absolute(),
            "the production Main nominal is 12 logical pixels"
        );
        let nominal_px = f64::from(
            context
                .load_font(&nominal)
                .unwrap()
                .describe_with_absolute_size()
                .size(),
        ) / f64::from(gtk::pango::SCALE);
        // Resolve point sizes independently; Pango's quantized point result
        // must not be treated as an absolute-pixel size or a rounded ratio.
        let point_14 = gtk::pango::FontDescription::from_string("sans-serif 14");
        let point_14_px = f64::from(
            context
                .load_font(&point_14)
                .unwrap()
                .describe_with_absolute_size()
                .size(),
        ) / f64::from(gtk::pango::SCALE);
        let point_28 = gtk::pango::FontDescription::from_string("sans-serif 28");
        let point_28_px = f64::from(
            context
                .load_font(&point_28)
                .unwrap()
                .describe_with_absolute_size()
                .size(),
        ) / f64::from(gtk::pango::SCALE);
        let point_14_ratio = point_14_px / nominal_px;
        let point_28_ratio = point_28_px / nominal_px;
        for (description, scale, expected_factor, ui) in [
            ("Monospace 12px", 1.0, 1.0, 1.0),
            ("Monospace 14", 1.0, point_14_ratio, 1.0),
            ("Monospace 28", 1.0, point_28_ratio, 1.0),
            ("Monospace 14px", 1.0, 14.0 / nominal_px, 1.0),
            ("Monospace 28px", 1.0, 28.0 / nominal_px, 1.0),
            ("Monospace 28", 1.3, point_28_ratio * 1.3, 1.0),
            ("Monospace 28px", 1.3, 36.4 / nominal_px, 1.0),
            ("Monospace 28", 10.0, point_28_ratio * 10.0, 1.0),
            ("Monospace 12px", 1.0, 1.0, 1.3),
            ("Monospace 12px", 1.0, 1.0, 1.0),
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

#[cfg(test)]
#[path = "workspace_panels_tests.rs"]
mod panel_tests;
