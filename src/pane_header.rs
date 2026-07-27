//! Per-pane status header and the drag-to-rearrange gesture built on it.
//!
//! Every pane's terminal is wrapped in a [`PaneFrame`]: a vertical box holding
//! a thin status strip above the terminal widget. The frame — not the terminal
//! — is what the `gtk::Paned` split tree contains, so all pane bookkeeping
//! (splitting, closing, zooming, session snapshots) addresses one widget per
//! pane exactly as before.
//!
//! The strip stays hidden while a tab holds a single pane: the tab bar and
//! window title already name it, and the row would only cost a terminal line.
//! Once a tab is split it shows each pane's number, title, working directory
//! and running command, and doubles as the handle for swapping two panes.

use relm4::gtk;
use relm4::gtk::prelude::*;
use relm4::gtk::{gdk, glib};

/// Drag payload: the stable pane id of the dragged pane. Ids never move between
/// panes, unlike the pane's index inside its tab.
///
/// Deliberately numeric rather than a string. A `gchararray` payload is exactly
/// what VTE's own text drop target accepts, so dropping a pane over the grid
/// would paste the id into the shell instead of rearranging the layout.
pub(crate) type PaneDragPayload = u64;

/// Style rules for the header strip. Installed alongside the app's other
/// static CSS.
pub(crate) const PANE_HEADER_CSS: &str = "
    .pane-header {
        padding: 1px 6px;
        border-bottom: 1px solid rgba(255,255,255,0.12);
        background-color: rgba(255,255,255,0.05);
    }
    .pane-header.pane-header-focused {
        background-color: rgba(80,160,255,0.20);
        border-bottom-color: rgba(80,160,255,0.65);
    }
    .pane-header.pane-header-drop {
        background-color: rgba(80,160,255,0.45);
        border-bottom-color: rgba(120,190,255,0.95);
    }
    .pane-header label { font-size: 9pt; }
    .pane-header-index { font-weight: bold; opacity: 0.9; }
    .pane-header-title { font-weight: bold; }
    .pane-header-cwd { opacity: 0.65; }
    .pane-header-command { color: #8be9fd; }
    .pane-frame-drop { outline: 2px solid rgba(120,190,255,0.9); outline-offset: -2px; }
";

/// One pane's chrome: the status strip plus the terminal beneath it.
pub(crate) struct PaneFrame {
    root: gtk::Box,
    header: gtk::Box,
    index: gtk::Label,
    title: gtk::Label,
    cwd: gtk::Label,
    command: gtk::Label,
}

impl PaneFrame {
    /// Wrap `terminal` in a frame. The header starts hidden, matching the
    /// single-pane case that every tab begins in.
    pub(crate) fn new(terminal: &gtk::Widget) -> Self {
        let index = gtk::Label::new(None);
        index.add_css_class("pane-header-index");

        let title = gtk::Label::new(None);
        title.add_css_class("pane-header-title");
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        title.set_xalign(0.0);

        let cwd = gtk::Label::new(None);
        cwd.add_css_class("pane-header-cwd");
        cwd.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        cwd.set_xalign(0.0);

        let command = gtk::Label::new(None);
        command.add_css_class("pane-header-command");
        command.set_ellipsize(gtk::pango::EllipsizeMode::End);
        command.set_xalign(0.0);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        header.add_css_class("pane-header");
        header.append(&index);
        header.append(&title);
        header.append(&cwd);
        header.append(&command);
        // The strip is the drag handle, so tell the user it is grabbable.
        header.set_cursor_from_name(Some("grab"));
        header.set_visible(false);
        header.set_tooltip_text(Some("Drag onto another pane to swap them"));

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.set_hexpand(true);
        root.set_vexpand(true);
        root.append(&header);
        root.append(terminal);

        PaneFrame {
            root,
            header,
            index,
            title,
            cwd,
            command,
        }
    }

    /// The widget the split tree holds for this pane.
    pub(crate) fn widget(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }

    /// Show the strip only while the tab is split.
    pub(crate) fn set_header_visible(&self, visible: bool) {
        self.header.set_visible(visible);
    }

    pub(crate) fn set_focused(&self, focused: bool) {
        if focused {
            self.header.add_css_class("pane-header-focused");
        } else {
            self.header.remove_css_class("pane-header-focused");
        }
    }

    /// Highlight this pane as the pane a drop would swap with.
    pub(crate) fn set_drop_target(&self, active: bool) {
        if active {
            self.header.add_css_class("pane-header-drop");
            self.root.add_css_class("pane-frame-drop");
        } else {
            self.header.remove_css_class("pane-header-drop");
            self.root.remove_css_class("pane-frame-drop");
        }
    }

    /// Fill in the strip. Empty fields are hidden rather than left blank, so a
    /// narrow pane spends its width on the fields that say something.
    pub(crate) fn set_status(
        &self,
        position: usize,
        title: &str,
        cwd: Option<&str>,
        command: Option<&str>,
    ) {
        self.index.set_text(&(position + 1).to_string());
        self.title.set_text(title);
        match cwd {
            // The title is usually the directory's last component; repeating
            // the whole path only earns its space when it differs.
            Some(cwd) if cwd != title => {
                self.cwd.set_text(cwd);
                self.cwd.set_visible(true);
            }
            _ => self.cwd.set_visible(false),
        }
        match command {
            Some(command) => {
                self.command.set_text(&format!("▶ {command}"));
                self.command.set_visible(true);
            }
            None => self.command.set_visible(false),
        }
    }

    /// Attach the drag handle to the strip and the drop zone to the whole
    /// frame, so a drop anywhere in the target pane counts.
    ///
    /// `on_drop` receives the dragged pane's id and returns whether the swap
    /// happened.
    pub(crate) fn install_drag_and_drop(
        &self,
        pane_id: PaneDragPayload,
        on_drop: impl Fn(PaneDragPayload) -> bool + 'static,
    ) {
        let source = gtk::DragSource::new();
        source.set_actions(gdk::DragAction::MOVE);
        source.connect_prepare(move |_, _, _| {
            Some(gdk::ContentProvider::for_value(&pane_id.to_value()))
        });
        self.header.add_controller(source);

        // The highlight closures hold the frame weakly. A strong capture would
        // make the frame own a controller that owns the frame, and GTK would
        // never free the pane — taking its PTY and scrollback with it.
        fn set_highlight(frame: &glib::WeakRef<gtk::Box>, on: bool) {
            if let Some(frame) = frame.upgrade() {
                if on {
                    frame.add_css_class("pane-frame-drop");
                } else {
                    frame.remove_css_class("pane-frame-drop");
                }
            }
        }

        let target = gtk::DropTarget::new(u64::static_type(), gdk::DragAction::MOVE);
        let frame = self.root.downgrade();
        target.connect_enter(move |_, _, _| {
            set_highlight(&frame, true);
            gdk::DragAction::MOVE
        });
        let frame = self.root.downgrade();
        target.connect_leave(move |_| set_highlight(&frame, false));
        let frame = self.root.downgrade();
        target.connect_drop(move |_, value, _, _| {
            set_highlight(&frame, false);
            match value.get::<PaneDragPayload>() {
                Ok(dragged) => on_drop(dragged),
                Err(_) => false,
            }
        });
        self.root.add_controller(target);
    }
}

/// Where a pane frame sits inside the split tree.
enum PaneSlot {
    Start(gtk::Paned),
    End(gtk::Paned),
}

impl PaneSlot {
    fn of(widget: &gtk::Widget) -> Option<Self> {
        let paned = widget.parent()?.downcast::<gtk::Paned>().ok()?;
        if paned.start_child().as_ref() == Some(widget) {
            Some(PaneSlot::Start(paned))
        } else if paned.end_child().as_ref() == Some(widget) {
            Some(PaneSlot::End(paned))
        } else {
            None
        }
    }

    fn clear(&self) {
        match self {
            PaneSlot::Start(paned) => paned.set_start_child(None::<&gtk::Widget>),
            PaneSlot::End(paned) => paned.set_end_child(None::<&gtk::Widget>),
        }
    }

    fn fill(&self, widget: &gtk::Widget) {
        match self {
            PaneSlot::Start(paned) => paned.set_start_child(Some(widget)),
            PaneSlot::End(paned) => paned.set_end_child(Some(widget)),
        }
    }
}

/// Exchange two panes' positions in the split tree, leaving the tree shape and
/// every divider position exactly as the user arranged them.
///
/// Both slots are cleared before either is refilled: handing GTK a widget that
/// still has a parent would leave the tree half-updated.
pub(crate) fn swap_pane_widgets(a: &gtk::Widget, b: &gtk::Widget) -> bool {
    if a == b {
        return false;
    }
    let (Some(a_slot), Some(b_slot)) = (PaneSlot::of(a), PaneSlot::of(b)) else {
        return false;
    };
    a_slot.clear();
    b_slot.clear();
    a_slot.fill(b);
    b_slot.fill(a);
    true
}
