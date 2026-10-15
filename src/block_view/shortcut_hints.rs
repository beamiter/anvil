//! Read-only shortcut hints. Callers must supply an exactly equivalent action.
use super::*;
use crate::keybindings::{Action, KeybindingMap};

pub(crate) type MenuKeybindings = Rc<RefCell<Option<Rc<RefCell<KeybindingMap>>>>>;

pub(crate) fn decorate(button: &gtk::Button, source: &MenuKeybindings, action: Action) {
    let source = source.borrow();
    let Some(map) = source.as_ref() else {
        return;
    };
    decorate_with_map(button, &map.borrow(), action);
}

pub(crate) fn decorate_with_map(button: &gtk::Button, map: &KeybindingMap, action: Action) {
    let shortcut = map.menu_binding_display(&action);
    if shortcut.is_empty() {
        return;
    }
    let title = button.label().unwrap_or_default();
    let body = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let label = gtk::Label::new(Some(&title));
    label.set_xalign(0.0);
    let hint = gtk::Label::new(Some(&shortcut));
    hint.add_css_class("dim-label");
    hint.add_css_class("block-menu-shortcut");
    hint.set_xalign(0.0);
    hint.set_ellipsize(gtk::pango::EllipsizeMode::End);
    hint.set_max_width_chars(32);
    body.append(&label);
    body.append(&hint);
    button.set_child(Some(&body));
    button.update_property(&[gtk::accessible::Property::Label(&format!(
        "{title}, {shortcut}"
    ))]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires DISPLAY; run explicitly under Xvfb"]
    fn menu_hints_follow_current_mapping_without_dispatch() {
        gtk::init().expect("GTK display");
        let map = Rc::new(RefCell::new(KeybindingMap::from_defaults()));
        let source = Rc::new(RefCell::new(Some(map.clone())));
        let first = gtk::Button::with_label("Paste");
        let clicks = Rc::new(Cell::new(0));
        let clicks_for_action = clicks.clone();
        first.connect_clicked(move |_| clicks_for_action.set(clicks_for_action.get() + 1));
        decorate(&first, &source, Action::Paste);
        let expected = map.borrow().menu_binding_display(&Action::Paste);
        let hint = first
            .child()
            .unwrap()
            .last_child()
            .and_downcast::<gtk::Label>()
            .unwrap();
        assert_eq!(hint.text().as_str(), expected);
        assert!(hint.has_css_class("block-menu-shortcut"));
        let mut overrides = toml::Table::new();
        overrides.insert("paste".into(), toml::Value::String("F10".into()));
        map.borrow_mut()
            .apply_user_overrides(&overrides)
            .expect("valid rebind");
        let rebound = gtk::Button::with_label("Paste");
        decorate(&rebound, &source, Action::Paste);
        let hint = rebound
            .child()
            .unwrap()
            .last_child()
            .and_downcast::<gtk::Label>()
            .unwrap();
        assert_eq!(hint.text(), "F10");
        overrides.insert("paste".into(), toml::Value::String("none".into()));
        map.borrow_mut()
            .apply_user_overrides(&overrides)
            .expect("valid unbind");
        let unbound = gtk::Button::with_label("Paste");
        decorate(&unbound, &source, Action::Paste);
        assert_eq!(unbound.label().as_deref(), Some("Paste"));
        assert_eq!(clicks.get(), 0, "rendering hints must not dispatch actions");
        first.emit_clicked();
        assert_eq!(clicks.get(), 1, "native action callback remains intact");
    }
}
