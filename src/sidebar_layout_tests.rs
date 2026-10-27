//! Exercise the production sidebar policy with an oversized inactive page.
use gtk::prelude::*;
use relm4::gtk;

#[test]
#[ignore = "requires a GTK display"]
fn sidebar_resize_and_vertical_scroll_keep_the_leading_edge_visible() {
    gtk::init().expect("GTK display");
    let tabs = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Filter tabs…"));
    tabs.append(&search);
    let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let mut rows = Vec::new();
    for i in 0..60 {
        let label = gtk::Label::new(Some(&format!("projects / terminal {i}")));
        label.set_xalign(0.0);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_height_request(30);
        list.append(&label);
        rows.push(label);
    }
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_vexpand(true);
    scroll.set_child(Some(&list));
    tabs.append(&scroll);
    let stack = gtk::Stack::new();
    stack.add_titled(&tabs, Some("tabs"), "Tabs");
    let files = gtk::Box::new(gtk::Orientation::Vertical, 0);
    files.set_width_request(700);
    stack.add_titled(&files, Some("files"), "Files");
    stack.set_visible_child_name("tabs");
    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let switcher = gtk::StackSwitcher::new();
    switcher.set_stack(Some(&stack));
    sidebar.append(&switcher);
    sidebar.append(&stack);
    let split = gtk::Paned::new(gtk::Orientation::Horizontal);
    split.set_start_child(Some(&sidebar));
    split.set_end_child(Some(&gtk::Box::new(gtk::Orientation::Vertical, 0)));
    super::configure_layout(&stack, &split);
    let window = gtk::Window::builder()
        .default_width(1000)
        .default_height(500)
        .child(&split)
        .build();
    window.present();
    let settle = || {
        for _ in 0..30 {
            while gtk::glib::MainContext::default().iteration(false) {}
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    };
    for width in [340, 120, 280] {
        split.set_position(width);
        settle();
        assert!(
            sidebar.width() < 700,
            "inactive Files page must not set the Tabs minimum"
        );
        for value in [0.0, 420.0, 900.0] {
            scroll.vadjustment().set_value(value);
            settle();
            for widget in [
                sidebar.upcast_ref::<gtk::Widget>(),
                switcher.upcast_ref(),
                search.upcast_ref(),
                list.upcast_ref(),
            ] {
                let bounds = widget.compute_bounds(&window).unwrap();
                assert!(
                    bounds.x() >= -0.5,
                    "sidebar content cannot disappear to the left after resize/scroll: {bounds:?}"
                );
            }
            assert!(scroll.hadjustment().value().abs() < 0.5);
        }
    }
    window.close();
}
