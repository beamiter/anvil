//! Bounded native context-menu viewport, shared by tab-menu entry points.
use gtk::prelude::*;
use relm4::gtk;

/// Keep small menus at their natural size; long or large-font menus remain
/// reachable inside the monitor. Horizontal scrolling preserves full labels.
pub(crate) fn scrolled_content(
    content: &impl IsA<gtk::Widget>,
    anchor: &impl IsA<gtk::Widget>,
) -> gtk::ScrolledWindow {
    let display = anchor.display();
    let monitor = anchor
        .native()
        .and_then(|native| native.surface())
        .and_then(|surface| display.monitor_at_surface(&surface))
        .or_else(|| {
            display
                .monitors()
                .item(0)
                .and_downcast::<gtk::gdk::Monitor>()
        });
    let (width, height) = monitor
        .map(|monitor| {
            let rect = monitor.geometry();
            (rect.width(), rect.height())
        })
        .unwrap_or_else(|| (anchor.width().max(1), anchor.height().max(1)));
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .propagate_natural_width(true)
        .propagate_natural_height(true)
        .max_content_width(width.saturating_mul(4).saturating_div(5).max(1))
        .max_content_height(height.saturating_mul(2).saturating_div(3).max(1))
        .child(content)
        .build()
}
