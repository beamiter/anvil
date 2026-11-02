//! This presentation toggle retains the original widgets and their live state.
use gtk::prelude::*;
use relm4::gtk;

pub(crate) fn group(
    content: &gtk::Widget,
    terminals: &gtk::Widget,
    terminal_content: &gtk::Widget,
    visible: bool,
    changed: impl Fn(bool) + 'static,
) -> gtk::Expander {
    let group = gtk::Expander::new(Some("Agent Workspace"));
    group.set_tooltip_text(Some("Show or hide Agent Workspace conversations"));
    group.set_child(Some(content));
    group.set_expanded(visible);
    apply_layout(&group, content, terminals, terminal_content);
    let (content, terminals, terminal_content) =
        (content.clone(), terminals.clone(), terminal_content.clone());
    group.connect_expanded_notify(move |group| {
        apply_layout(group, &content, &terminals, &terminal_content);
        changed(group.is_expanded());
    });
    group
}

fn apply_layout(
    group: &gtk::Expander,
    content: &gtk::Widget,
    terminals: &gtk::Widget,
    terminal_content: &gtk::Widget,
) {
    let visible = group.is_expanded();
    group.set_vexpand(visible);
    content.set_vexpand(visible);
    terminals.set_vexpand(!visible);
    terminal_content.set_vexpand(!visible);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn collapse_retains_conversation_widgets_and_gives_terminals_the_space() {
        if gtk::init().is_err() {
            return;
        }
        let entry = gtk::Entry::new();
        entry.set_text("same conversation draft");
        entry.set_width_request(312);
        let adjustment = gtk::Adjustment::new(42.0, 0.0, 1000.0, 1.0, 10.0, 100.0);
        let conversations = gtk::ScrolledWindow::new();
        conversations.set_vadjustment(Some(&adjustment));
        conversations.set_child(Some(&entry));
        let tabs = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let terminals = gtk::Expander::new(Some("Terminals"));
        terminals.set_child(Some(&tabs));
        let changes = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let recorded = changes.clone();
        let group = group(
            conversations.upcast_ref(),
            terminals.upcast_ref(),
            tabs.upcast_ref(),
            true,
            move |visible| recorded.borrow_mut().push(visible),
        );
        group.set_expanded(false);
        assert!(!group.vexpands());
        assert!(tabs.vexpands());
        assert!(terminals.vexpands());
        assert_eq!(entry.text().as_str(), "same conversation draft");
        assert_eq!(entry.width_request(), 312);
        assert_eq!(adjustment.value(), 42.0);
        group.set_expanded(true);
        assert!(group.vexpands());
        assert!(!tabs.vexpands());
        assert_eq!(group.child().as_ref(), Some(conversations.upcast_ref()));
        assert_eq!(entry.text().as_str(), "same conversation draft");
        assert_eq!(&*changes.borrow(), &[false, true]);
    }
}
