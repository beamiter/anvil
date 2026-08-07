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

/// Stable workspace identity carried over GTK drag-and-drop.
///
/// Tabs and panes used to both publish a bare `u64`, so a pane target could
/// mistake a tab id for a pane id. A private boxed GType both distinguishes the
/// two kinds and prevents VTE's text drop target from claiming the payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkspaceDragItem {
    Tab(u64),
    Pane(u64),
}

#[derive(Clone, Debug, PartialEq, Eq, glib::Boxed)]
#[boxed_type(name = "AnvilWorkspaceDragPayload")]
pub(crate) struct WorkspaceDragPayload(WorkspaceDragItem);

impl WorkspaceDragPayload {
    pub(crate) fn tab(id: u64) -> Self {
        Self(WorkspaceDragItem::Tab(id))
    }

    pub(crate) fn pane(id: u64) -> Self {
        Self(WorkspaceDragItem::Pane(id))
    }

    pub(crate) fn item(&self) -> WorkspaceDragItem {
        self.0
    }
}

/// Edge of a pane where a dropped ordinary tab becomes a split sibling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaneDropEdge {
    Left,
    Right,
    Top,
    Bottom,
}

const DROP_EDGE_FRACTION: f64 = 0.25;

/// Resolve a four-way edge zone. The center is deliberately not a target, so
/// an imprecise/cancelled drag cannot rearrange the workspace.
pub(crate) fn pane_drop_edge(x: f64, y: f64, width: i32, height: i32) -> Option<PaneDropEdge> {
    if width <= 0
        || height <= 0
        || !x.is_finite()
        || !y.is_finite()
        || x < 0.0
        || y < 0.0
        || x > f64::from(width)
        || y > f64::from(height)
    {
        return None;
    }
    let x = x / f64::from(width);
    let y = y / f64::from(height);
    let (edge, distance) = [
        (PaneDropEdge::Left, x),
        (PaneDropEdge::Right, 1.0 - x),
        (PaneDropEdge::Top, y),
        (PaneDropEdge::Bottom, 1.0 - y),
    ]
    .into_iter()
    .min_by(|(_, a), (_, b)| a.total_cmp(b))?;
    (distance <= DROP_EDGE_FRACTION).then_some(edge)
}

/// `Some(None)` accepts a pane swap with a neutral outline,
/// `Some(Some(edge))` accepts a directional tab split, and `None` rejects the
/// point entirely. Keeping those three states distinct prevents a tab over the
/// dead center from advertising MOVE like a pane swap.
fn workspace_drop_preview(
    item: WorkspaceDragItem,
    x: f64,
    y: f64,
    width: i32,
    height: i32,
) -> Option<Option<PaneDropEdge>> {
    match item {
        WorkspaceDragItem::Tab(_) => pane_drop_edge(x, y, width, height).map(Some),
        // Pane-on-pane is a swap. A directional shadow would promise a split
        // that the drop handler deliberately does not perform.
        WorkspaceDragItem::Pane(_) => Some(None),
    }
}

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
    .pane-frame-drop-left { box-shadow: inset 6px 0 rgba(120,190,255,0.85); }
    .pane-frame-drop-right { box-shadow: inset -6px 0 rgba(120,190,255,0.85); }
    .pane-frame-drop-top { box-shadow: inset 0 6px rgba(120,190,255,0.85); }
    .pane-frame-drop-bottom { box-shadow: inset 0 -6px rgba(120,190,255,0.85); }
    .pane-to-tab-drop { outline: 2px solid rgba(120,190,255,0.9); outline-offset: -2px; }
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
        header.set_tooltip_text(Some(
            "Drag onto another pane to swap, or onto the tab bar to make a tab",
        ));

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

    /// Attach the pane drag handle to the strip and a typed workspace drop
    /// target to the whole frame. Pane payloads may swap anywhere; tab payloads
    /// are accepted only by the directional edge selected at the drop point.
    pub(crate) fn install_drag_and_drop(
        &self,
        pane_id: u64,
        on_drop: impl Fn(WorkspaceDragItem, Option<PaneDropEdge>) -> bool + 'static,
    ) {
        let source = gtk::DragSource::new();
        source.set_actions(gdk::DragAction::MOVE);
        source.connect_prepare(move |_, _, _| {
            let payload = WorkspaceDragPayload::pane(pane_id);
            Some(gdk::ContentProvider::for_value(&payload.to_value()))
        });
        self.header.add_controller(source);

        // The highlight closures hold the frame weakly. A strong capture would
        // make the frame own a controller that owns the frame, and GTK would
        // never free the pane — taking its PTY and scrollback with it.
        fn set_highlight(frame: &glib::WeakRef<gtk::Box>, on: bool, edge: Option<PaneDropEdge>) {
            if let Some(frame) = frame.upgrade() {
                for class in [
                    "pane-frame-drop-left",
                    "pane-frame-drop-right",
                    "pane-frame-drop-top",
                    "pane-frame-drop-bottom",
                ] {
                    frame.remove_css_class(class);
                }
                if on {
                    frame.add_css_class("pane-frame-drop");
                    let class = match edge {
                        Some(PaneDropEdge::Left) => Some("pane-frame-drop-left"),
                        Some(PaneDropEdge::Right) => Some("pane-frame-drop-right"),
                        Some(PaneDropEdge::Top) => Some("pane-frame-drop-top"),
                        Some(PaneDropEdge::Bottom) => Some("pane-frame-drop-bottom"),
                        None => None,
                    };
                    if let Some(class) = class {
                        frame.add_css_class(class);
                    }
                } else {
                    frame.remove_css_class("pane-frame-drop");
                }
            }
        }

        let target =
            gtk::DropTarget::new(WorkspaceDragPayload::static_type(), gdk::DragAction::MOVE);
        target.set_preload(true);
        let frame = self.root.downgrade();
        target.connect_enter(move |target, x, y| {
            let preview = target
                .value()
                .and_then(|value| value.get::<WorkspaceDragPayload>().ok())
                .and_then(|payload| {
                    frame.upgrade().and_then(|frame| {
                        workspace_drop_preview(payload.item(), x, y, frame.width(), frame.height())
                    })
                });
            if let Some(edge) = preview {
                set_highlight(&frame, true, edge);
                gdk::DragAction::MOVE
            } else {
                set_highlight(&frame, false, None);
                gdk::DragAction::empty()
            }
        });
        let frame = self.root.downgrade();
        target.connect_motion(move |target, x, y| {
            let preview = target
                .value()
                .and_then(|value| value.get::<WorkspaceDragPayload>().ok())
                .and_then(|payload| {
                    frame.upgrade().and_then(|frame| {
                        workspace_drop_preview(payload.item(), x, y, frame.width(), frame.height())
                    })
                });
            if let Some(edge) = preview {
                set_highlight(&frame, true, edge);
                gdk::DragAction::MOVE
            } else {
                set_highlight(&frame, false, None);
                gdk::DragAction::empty()
            }
        });
        let frame = self.root.downgrade();
        target.connect_leave(move |_| set_highlight(&frame, false, None));
        let frame = self.root.downgrade();
        target.connect_drop(move |_, value, x, y| {
            set_highlight(&frame, false, None);
            match value.get::<WorkspaceDragPayload>() {
                Ok(payload) => frame
                    .upgrade()
                    .and_then(|frame| {
                        workspace_drop_preview(payload.item(), x, y, frame.width(), frame.height())
                    })
                    .is_some_and(|edge| on_drop(payload.item(), edge)),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_payload_is_typed_and_round_trips_both_stable_id_kinds() {
        assert_ne!(WorkspaceDragPayload::static_type(), u64::static_type());
        assert_ne!(WorkspaceDragPayload::static_type(), String::static_type());

        for expected in [WorkspaceDragItem::Tab(41), WorkspaceDragItem::Pane(73)] {
            let payload = match expected {
                WorkspaceDragItem::Tab(id) => WorkspaceDragPayload::tab(id),
                WorkspaceDragItem::Pane(id) => WorkspaceDragPayload::pane(id),
            };
            let restored = payload
                .to_value()
                .get::<WorkspaceDragPayload>()
                .expect("private boxed drag payload should round-trip");
            assert_eq!(restored.item(), expected);
        }
    }

    #[test]
    fn pane_drop_zones_cover_four_edges_but_not_the_center() {
        assert_eq!(
            pane_drop_edge(1.0, 50.0, 100, 100),
            Some(PaneDropEdge::Left)
        );
        assert_eq!(
            pane_drop_edge(99.0, 50.0, 100, 100),
            Some(PaneDropEdge::Right)
        );
        assert_eq!(pane_drop_edge(50.0, 1.0, 100, 100), Some(PaneDropEdge::Top));
        assert_eq!(
            pane_drop_edge(50.0, 99.0, 100, 100),
            Some(PaneDropEdge::Bottom)
        );
        assert_eq!(pane_drop_edge(50.0, 50.0, 100, 100), None);
    }

    #[test]
    fn pane_drop_zone_rejects_invalid_or_outside_geometry() {
        assert_eq!(pane_drop_edge(-1.0, 5.0, 100, 100), None);
        assert_eq!(pane_drop_edge(5.0, 101.0, 100, 100), None);
        assert_eq!(pane_drop_edge(f64::NAN, 5.0, 100, 100), None);
        assert_eq!(pane_drop_edge(5.0, 5.0, 0, 100), None);
    }

    #[test]
    fn pane_swap_preview_never_claims_a_directional_split_edge() {
        assert_eq!(
            workspace_drop_preview(WorkspaceDragItem::Pane(7), 50.0, 50.0, 100, 100),
            Some(None)
        );
        assert_eq!(
            workspace_drop_preview(WorkspaceDragItem::Tab(7), 1.0, 50.0, 100, 100),
            Some(Some(PaneDropEdge::Left))
        );
        assert_eq!(
            workspace_drop_preview(WorkspaceDragItem::Tab(7), 50.0, 50.0, 100, 100),
            None
        );
    }
}
