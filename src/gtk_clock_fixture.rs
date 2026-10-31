//! Ignored native integration fixture for the exact production GTK adapter.
//! Only ordinary window lifecycle operations are controlled; no protocol,
//! frame timing, grant or retirement messages are fabricated.
use super::gtk;
use gtk::prelude::*;
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

fn native_state(window: &gtk::Window) -> Value {
    json!({
        "mapped": window.is_mapped(),
        "realized": window.is_realized(),
        "surface": window.surface().map(|surface| surface.as_ptr() as usize),
        "clock": window.frame_clock().map(|clock| clock.as_ptr() as usize),
        "frame": window.frame_clock().map(|clock| clock.frame_counter()),
    })
}

#[test]
#[ignore = "requires private native display, real coordinator and window-lifecycle driver"]
fn native_clock_recreation_uses_exact_adapter() {
    let control = PathBuf::from(
        std::env::var_os("EVA_GTK_CLOCK_CONTROL").expect("private lifecycle control"),
    );
    let report =
        PathBuf::from(std::env::var_os("EVA_GTK_CLOCK_STATUS").expect("private native status"));
    let frontend = std::env::var("EVA_GTK_CLOCK_FRONTEND").expect("frontend name");
    assert!(matches!(frontend.as_str(), "forge" | "anvil"));
    let _ = env_logger::Builder::from_env(env_logger::Env::default())
        .is_test(false)
        .try_init();
    gtk::init().expect("private GTK display");
    super::configure(true);
    let provider = gtk::CssProvider::new();
    provider.load_from_string("window.clock-fixture { background: #001b23; color: #eeeeee; }");
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().unwrap(),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    let title = format!(
        "{} clock",
        if frontend == "forge" {
            "Forge"
        } else {
            "Anvil"
        }
    );
    let primary = gtk::Window::builder()
        .title(format!("{title} primary"))
        .default_width(1344)
        .default_height(1100)
        .build();
    let secondary = gtk::Window::builder()
        .title(format!("{title} secondary"))
        .default_width(1344)
        .default_height(1100)
        .build();
    for window in [&primary, &secondary] {
        window.add_css_class("clock-fixture");
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&gtk::Label::new(Some(
            "Native GTK window lifecycle fixture",
        ))));
        window.set_child(Some(&overlay));
        super::install(window, &overlay, &frontend);
        window.present();
    }
    let main_loop = gtk::glib::MainLoop::new(None, false);
    let quit = main_loop.clone();
    let p = primary.clone();
    let s = secondary.clone();
    let started = Instant::now();
    let mut nonce = 0;
    let mut last_action = String::new();
    let mut replaced = Value::Null;
    // Retain the old objects so numerical pointer reuse cannot disguise a
    // different native surface or frame clock in the external assertions.
    let mut old_surface = None::<gtk::gdk::Surface>;
    let mut old_clock = None::<gtk::gdk::FrameClock>;
    gtk::glib::timeout_add_local(Duration::from_millis(25), move || {
        if let Ok(bytes) = std::fs::read(&control) {
            if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                if let Some(next) = value["nonce"].as_u64().filter(|next| *next > nonce) {
                    nonce = next;
                    last_action = value["action"].as_str().unwrap_or_default().to_owned();
                    match last_action.as_str() {
                        "rebuild-primary" => {
                            replaced = native_state(&p);
                            old_surface = p.surface();
                            old_clock = p.frame_clock();
                            p.set_visible(false);
                            gtk::prelude::WidgetExt::unrealize(&p);
                        }
                        "hide-primary" => p.set_visible(false),
                        "show-primary" => p.present(),
                        "finish" => quit.quit(),
                        _ => panic!("unknown native lifecycle action"),
                    }
                }
            }
        }
        if p.is_mapped() {
            p.queue_draw();
        }
        if s.is_mapped() {
            s.queue_draw();
        }
        let state = json!({
            "pid": std::process::id(), "nonce": nonce, "last_action": last_action,
            "primary": native_state(&p), "secondary": native_state(&s),
            "replaced": replaced,
            "old_surface_destroyed": old_surface.as_ref().map(|surface| surface.is_destroyed()),
            "old_clock": old_clock.as_ref().map(|clock| clock.as_ptr() as usize),
        });
        let pending = report.with_extension("next");
        std::fs::write(&pending, serde_json::to_vec(&state).unwrap()).unwrap();
        std::fs::rename(pending, &report).unwrap();
        if last_action == "finish" || started.elapsed() > Duration::from_secs(120) {
            quit.quit();
            gtk::glib::ControlFlow::Break
        } else {
            gtk::glib::ControlFlow::Continue
        }
    });
    main_loop.run();
    primary.destroy();
    secondary.destroy();
    assert!(
        started.elapsed() < Duration::from_secs(120),
        "native lifecycle driver did not finish"
    );
}
