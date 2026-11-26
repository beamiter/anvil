//! Link interaction for the historical read-only terminal renderer.
use gtk::prelude::*;
use relm4::gtk;
use vte4::{Terminal, TerminalExt};

/// Ctrl+Click on a link opens it — a URL the regex matches or an OSC 8 target
/// the opener accepts; other clicks pass through to VTE selection and to a
/// program's mouse reporting. Hovering an OSC 8 link shows its target.
pub(crate) fn setup_terminal_click_handler(terminal: &Terminal) {
    super::url::show_hyperlink_target_on_hover(terminal);
    crate::vte_reference::install_hover(terminal);
    let weak = terminal.downgrade();
    let opener = super::url::LinkOpener::new(terminal);
    let on_changed = opener.clone();
    terminal.connect_contents_changed(move |_| on_changed.invalidate());
    crate::vte_reference::click(
        terminal,
        move |x, y| super::url::openable_link_at(&weak.upgrade()?, x, y),
        move |uri| opener.open(&uri),
    );
}
