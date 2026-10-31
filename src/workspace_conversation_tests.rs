//! Native bounds for the real Current Step renderer, with production CSS.
use super::*;
use std::time::{Duration, Instant};

fn settle() {
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(180) {
        while gtk::glib::MainContext::default().iteration(false) {}
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn find(widget: &gtk::Widget, class: &str) -> gtk::Widget {
    if widget.has_css_class(class) {
        return widget.clone();
    }
    let mut child = widget.first_child();
    while let Some(widget) = child {
        if let Some(found) = find_optional(&widget, class) {
            return found;
        }
        child = widget.next_sibling();
    }
    panic!("missing actual component {class}");
}

fn find_optional(widget: &gtk::Widget, class: &str) -> Option<gtk::Widget> {
    if widget.has_css_class(class) {
        return Some(widget.clone());
    }
    let mut child = widget.first_child();
    while let Some(widget) = child {
        if let Some(found) = find_optional(&widget, class) {
            return Some(found);
        }
        child = widget.next_sibling();
    }
    None
}

fn bounds(widget: &impl IsA<gtk::Widget>, window: &gtk::Window) -> gtk::graphene::Rect {
    widget.compute_bounds(window).expect("mapped native bounds")
}

fn center(rect: &gtk::graphene::Rect) -> f32 {
    rect.y() + rect.height() / 2.0
}

#[test]
#[ignore = "requires isolated DISPLAY; native user messages and production CSS"]
fn user_capsules_follow_reading_edge_and_wrap_in_narrow_windows() {
    gtk::init().expect("GTK display");
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("workspace_shell.css"));
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().expect("display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    let document = gtk::Box::new(gtk::Orientation::Vertical, 11);
    document.add_css_class("native-conversation");
    let values = ["pwd".to_owned(), "LongToken界面".repeat(80)];
    let mut rows = Vec::new();
    for value in &values {
        let mut message = jterm_core::reference_conversation::acceptance_fixture()
            .messages
            .into_iter()
            .find(|message| message.role == Role::User)
            .expect("fixture user message");
        message.parts = vec![Part::Paragraph(value.clone())];
        let row = message_view(&message);
        document.append(&row);
        rows.push(row);
    }
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_child(Some(&document));
    let window = gtk::Window::builder()
        .default_width(900)
        .default_height(480)
        .child(&scroll)
        .build();
    window.present();
    for width in [900, 320, 640] {
        window.set_default_size(width, 480);
        settle();
        assert!(
            window.width() <= width + 1,
            "user messages must not impose their old fixed width"
        );
        for (index, row) in rows.iter().enumerate() {
            let rectangle = row.compute_bounds(&document).expect("native message bounds");
            assert!(rectangle.x().abs() <= 1.0, "shared reading edge");
            assert!(
                rectangle.x() + rectangle.width() <= document.width() as f32 + 1.0,
                "the complete message, including avatar, stays inside the pane"
            );
            let body = find(row.upcast_ref(), "conversation-user-bubble");
            let label = find(&body, "conversation-paragraph")
                .downcast::<gtk::Label>()
                .unwrap();
            assert_eq!(label.text().as_str(), values[index].as_str());
            assert!(
                !label.layout().is_ellipsized(),
                "messages wrap without truncation"
            );
            let (_, logical) = label.layout().pixel_extents();
            assert!(
                logical.height() <= label.height(),
                "all wrapped lines remain visible"
            );
            if index == 0 {
                assert!(
                    body.width() < document.width() / 2,
                    "short messages retain a compact content-sized capsule"
                );
            } else {
                assert!(label.layout().line_count() > 1, "long tokens and CJK wrap");
            }
        }
    }
    window.close();
}

#[test]
#[ignore = "requires isolated DISPLAY; actual Current Step component and production CSS"]
fn current_step_centers_bounded_text_without_growing_the_inspector() {
    gtk::init().expect("GTK display");
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("workspace_shell.css"));
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().expect("display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    for long in [false, true] {
        for height in [850, 613] {
            let mut model = jterm_core::reference_conversation::acceptance_fixture().inspector;
            let step = model.current_step.as_mut().expect("fixture current step");
            if long {
                step.title =
                    "Long current step title: ".to_owned() + &"retained canvas 界 ".repeat(40);
                step.detail = "Long current step detail: ".to_owned()
                    + &"keep the native content readable and bounded 界 ".repeat(40);
            }
            let title_text = step.title.clone();
            let detail_text = step.detail.clone();
            let window = gtk::Window::builder()
                .title("Anvil Current Step visual acceptance fixture")
                .default_width(306)
                .default_height(height)
                .decorated(false)
                .build();
            window.add_css_class("workspace-inspector");
            let scroll = gtk::ScrolledWindow::new();
            scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
            let view = inspector(&model, true);
            scroll.set_child(Some(&view));
            window.set_child(Some(&scroll));
            window.present();
            let start = Instant::now();
            while (!window.is_mapped() || view.width() < 250)
                && start.elapsed() < Duration::from_secs(3)
            {
                settle();
            }
            settle();
            assert!(window.is_mapped());
            let window_bounds = bounds(&window, &window);
            let scroll_bounds = bounds(&scroll, &window);
            let surface = window.surface().expect("native surface");
            let native_id = std::process::Command::new("xdotool")
                .args([
                    "search",
                    "--name",
                    "^Anvil Current Step visual acceptance fixture$",
                ])
                .output()
                .expect("native X window ID");
            assert!(native_id.status.success());
            let native_id = String::from_utf8(native_id.stdout).unwrap();
            let native_id = native_id.lines().last().unwrap();
            let physical = std::process::Command::new("xdotool")
                .args(["getwindowgeometry", "--shell", native_id])
                .output()
                .expect("native client geometry");
            assert!(physical.status.success());
            let physical = String::from_utf8(physical.stdout).unwrap();
            println!(
                "current_step_fixture_root {}",
                serde_json::json!({
                    "long":long,"height":height,
                    "widget_content":[window.width(),window.height()],
                    "window_bounds":[window_bounds.x(),window_bounds.y(),window_bounds.width(),window_bounds.height()],
                    "scroll_bounds":[scroll_bounds.x(),scroll_bounds.y(),scroll_bounds.width(),scroll_bounds.height()],
                    "surface":[surface.width(),surface.height()],"physical":physical
                })
            );
            assert_eq!((window.width(), window.height()), (305, height));
            assert_eq!((surface.width(), surface.height()), (306, height));
            assert_eq!(window_bounds.width(), 306.0);
            assert_eq!(scroll_bounds.width(), 305.0);
            assert!(physical.lines().any(|line| line == "WIDTH=306"));
            assert!(physical
                .lines()
                .any(|line| line == format!("HEIGHT={height}")));
            let card = find(view.upcast_ref(), "reference-inspector-current");
            let symbol = find(&card, "reference-step-symbol");
            let title = find(&card, "reference-step-title")
                .downcast::<gtk::Label>()
                .unwrap();
            let detail = find(&card, "reference-step-detail")
                .downcast::<gtk::Label>()
                .unwrap();
            let content = title.parent().expect("content column");
            let c = bounds(&card, &window);
            let s = bounds(&symbol, &window);
            let t = bounds(&title, &window);
            let d = bounds(&detail, &window);
            let b = bounds(&content, &window);
            let queue = bounds(
                &find(view.upcast_ref(), "reference-inspector-queue"),
                &window,
            );
            let result = serde_json::json!({
                "long":long,"height":height,"card":[c.x(),c.y(),c.width(),c.height()],
                "symbol":[s.x(),s.y(),s.width(),s.height()],
                "content":[b.x(),b.y(),b.width(),b.height()],
                "title":[t.x(),t.y(),t.width(),t.height()],
                "detail":[d.x(),d.y(),d.width(),d.height()],
                "lines":[title.layout().line_count(),detail.layout().line_count()],
                "ellipsized":[title.layout().is_ellipsized(),detail.layout().is_ellipsized()]
            });
            println!("current_step_native {result}");
            assert_eq!((c.width(), c.height()), (251.0, 45.0));
            assert!((center(&s) - center(&b)).abs() <= 1.0);
            assert!((center(&b) - center(&c)).abs() <= 1.0);
            // New shared 45px card replaces the 59px variant. Two bounded
            // lines retain at least 8px above/below, including the CJK case.
            assert!(b.y() - c.y() >= 8.0 && c.y() + c.height() - b.y() - b.height() >= 8.0);
            assert!(s.x() + s.width() + 8.0 <= b.x() + 0.5);
            assert!(t.y() + t.height() + 3.0 <= d.y() + 0.5);
            assert!(d.y() + d.height() <= c.y() + c.height() - 1.0);
            assert!(b.x() + b.width() <= c.x() + c.width() - 1.0);
            assert!(queue.y() > c.y() + c.height());
            assert_eq!(title.layout().line_count(), 1);
            assert!(detail.layout().line_count() <= 2);
            assert_eq!(
                detail.layout().line_count(),
                1,
                "title and description share the two-line role"
            );
            assert_eq!(title.tooltip_text().as_deref(), Some(title_text.as_str()));
            assert_eq!(detail.tooltip_text().as_deref(), Some(detail_text.as_str()));
            if long {
                assert!(title.layout().is_ellipsized());
                assert!(detail.layout().is_ellipsized());
            }
            for label in [&title, &detail] {
                let (_, logical) = label.layout().pixel_extents();
                assert!(logical.height() <= bounds(label, &window).height().ceil() as i32);
            }
            if let Some(dir) = std::env::var_os("ANVIL_CURRENT_STEP_EVIDENCE") {
                let name = format!("{}-{height}.png", if long { "long" } else { "normal" });
                let id = std::process::Command::new("xdotool")
                    .args([
                        "search",
                        "--name",
                        "^Anvil Current Step visual acceptance fixture$",
                    ])
                    .output()
                    .expect("native X window ID");
                assert!(id.status.success());
                let id = String::from_utf8(id.stdout).unwrap();
                let id = id.lines().last().unwrap();
                assert!(std::process::Command::new("import")
                    .args(["-window", id])
                    .arg(std::path::PathBuf::from(dir).join(name))
                    .status()
                    .expect("native pixels")
                    .success());
            }
            let adj = scroll.vadjustment();
            adj.set_value(adj.upper() - adj.page_size());
            settle();
            let output = bounds(
                &find(view.upcast_ref(), "reference-inspector-output"),
                &window,
            );
            assert_eq!(output.height(), 140.0);
            assert!(output.y() >= 0.0 && output.y() + output.height() <= height as f32);
            adj.set_value(0.0);
            settle();
            assert_eq!(bounds(&card, &window), c);
            window.close();
            settle();
        }
    }
}
