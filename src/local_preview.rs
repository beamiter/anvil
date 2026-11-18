//! Confirmation-first, nonexecuting local source preview. Unknown live VTE
//! ownership remains with the application; source text never opens an editor.
use crate::{local_preview_file, local_reference};
use gtk4::prelude::*;
use relm4::gtk as gtk4;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use vte4::{prelude::TerminalExtManual, TerminalExt};

static READER_BUSY: AtomicBool = AtomicBool::new(false);
const PAGE: &str = "local-source-preview";
#[derive(Default)]
struct PreviewState {
    generation: Cell<u64>,
    previous: RefCell<String>,
    restore: RefCell<Option<Box<dyn Fn()>>>,
}

fn candidate(raw: &str) -> Option<PathBuf> {
    if raw.len() > local_reference::MAX_REFERENCE_BYTES {
        return None;
    }
    let path = if raw.starts_with("file:///") || raw.starts_with("file://localhost/") {
        gtk4::gio::File::for_uri(raw).path()?.to_str()?.to_owned()
    } else {
        raw.to_owned()
    };
    let parsed = local_reference::parse_candidate(&path)?;
    let path = PathBuf::from(&parsed.exact_path);
    let plausible = path.is_absolute()
        || parsed.exact_path.contains('/')
        || super::local_preview_tokens::is_bare_source_name(&parsed.exact_path);
    (plausible
        && !parsed.exact_path.contains('$')
        && !path.starts_with("~")
        && !path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir)))
    .then_some(path)
}

thread_local! {
    // Weak registrations never keep a closed pane or its PTY alive.
    static OWNERS: RefCell<Vec<std::rc::Weak<crate::block_view::TermView>>> = const { RefCell::new(Vec::new()) };
}
pub(crate) fn register_owner(view: &Rc<crate::block_view::TermView>) {
    OWNERS.with(|owners| {
        let mut owners = owners.borrow_mut();
        owners.retain(|owner| owner.strong_count() > 0);
        owners.push(Rc::downgrade(view));
    });
}
fn host_owns_click(terminal: &vte4::Terminal) -> bool {
    if !terminal.is_input_enabled() {
        return true;
    }
    OWNERS.with(|owners| {
        owners
            .borrow()
            .iter()
            .filter_map(std::rc::Weak::upgrade)
            .any(|view| view.vte() == terminal && view.host_owns_local_preview_click())
    })
}
struct PreviewUi {
    side_stack: gtk4::Stack,
    show: Box<dyn Fn()>,
    capture: Box<dyn Fn() -> Box<dyn Fn()>>,
}

fn hit(window: &gtk4::Widget, x: f64, y: f64) -> Option<(usize, PathBuf)> {
    let mut picked = window.pick(x, y, gtk4::PickFlags::DEFAULT)?;
    let terminal = loop {
        if let Ok(terminal) = picked.clone().downcast::<vte4::Terminal>() {
            break terminal;
        }
        picked = picked.parent()?;
    };
    if !host_owns_click(&terminal) {
        return None;
    }
    let point = window.compute_point(&terminal, &gtk4::graphene::Point::new(x as f32, y as f32))?;
    // Explicit hyperlinks and the existing URL matcher take priority; never
    // reinterpret a refused remote OSC8 label as a local path.
    let raw = terminal
        .check_hyperlink_at(point.x().into(), point.y().into())
        .map(|s| s.to_string())
        .or_else(|| {
            crate::vte_reference::hit(&terminal, point.x().into(), point.y().into())
                .map(|reference| reference.target)
        })
        .or_else(|| {
            terminal
                .check_match_at(point.x().into(), point.y().into())
                .0
                .map(|s| s.to_string())
        });
    let owner = terminal.as_ptr() as usize;
    if let Some(raw) = raw {
        return candidate(&raw).map(|path| (owner, path));
    }
    // Click-local probing does not register a competing URL/selection matcher.
    thread_local! {
        static LOCAL_REFERENCE: Option<vte4::Regex> = vte4::Regex::for_match(
            super::local_preview_tokens::PATTERN, pcre2_sys::PCRE2_MULTILINE).ok();
    }
    LOCAL_REFERENCE.with(|regex| {
        terminal
            .check_regex_simple_at(point.x().into(), point.y().into(), &[regex.as_ref()?], 0)
            .first()
            .and_then(|raw| candidate(raw))
            .map(|path| (owner, path))
    })
}

fn show_candidate(ui: &Rc<PreviewUi>, state: &Rc<PreviewState>, path: PathBuf) {
    if state.restore.borrow().is_none() {
        *state.restore.borrow_mut() = Some((ui.capture)());
    }
    let token = state.generation.get().wrapping_add(1);
    state.generation.set(token);
    if ui.side_stack.visible_child_name().as_deref() != Some(PAGE) {
        *state.previous.borrow_mut() = ui
            .side_stack
            .visible_child_name()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "overview".into());
    }
    // Every candidate gets fresh buttons with immutable path/token ownership.
    // Old mouse/keyboard activations cannot authorize a replacement candidate.
    if let Some(old) = ui.side_stack.child_by_name(PAGE) {
        ui.side_stack.remove(&old);
    }
    let panel = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    let title = gtk4::Label::new(Some("Local file preview"));
    let path_label = gtk4::Label::new(Some(&path.to_string_lossy()));
    path_label.set_wrap(true);
    path_label.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
    path_label.set_max_width_chars(40);
    path_label.set_selectable(true);
    let warning = gtk4::Label::new(Some("Output does not prove this file is local. Confirm before reading this machine. Read-only UTF-8 source up to 256 KiB, or PNG/JPEG up to 4 MiB and 4 million pixels. No code is executed."));
    warning.set_wrap(true);
    warning.set_max_width_chars(40);
    let text = gtk4::TextView::new();
    let picture = gtk4::Picture::new();
    picture.set_can_shrink(true);
    picture.set_content_fit(gtk4::ContentFit::Contain);
    picture.set_visible(false);
    let directory = gtk4::Entry::new();
    directory.set_placeholder_text(Some("Absolute local directory, e.g. /home/me/project"));
    directory.set_tooltip_text(Some(
        "Type the actual directory on this machine. Terminal cwd is never used.",
    ));
    directory.set_max_length(4096);
    directory.set_visible(!path.is_absolute());
    let resolved = gtk4::Label::new(None);
    resolved.set_wrap(true);
    resolved.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
    resolved.set_max_width_chars(40);
    let update_path = path.clone();
    let weak_resolved = resolved.downgrade();
    let weak_preview_buffer = text.buffer().downgrade();
    directory.connect_changed(move |entry| {
        if let Some(buffer) = weak_preview_buffer.upgrade() {
            buffer.set_text("Confirm to read the selected local target");
        }
        if let Some(label) = weak_resolved.upgrade() {
            label.set_text(&match local_preview_file::selected_local_path(
                &update_path,
                entry.text().as_str(),
            ) {
                Ok(path) => format!("Local file: {}", path.display()),
                Err(_) => "Choose an absolute local directory before confirming".into(),
            });
        }
    });
    resolved.set_text(if path.is_absolute() {
        "Read only the local file shown above"
    } else {
        "Choose an absolute local directory before confirming"
    });
    // Preparation cannot read. It freezes the complete target before a separate
    // confirmation, so a queued activation never authorizes a changed directory.
    let prepared = Rc::new(std::cell::OnceCell::<PathBuf>::new());
    if path.is_absolute() {
        let _ = prepared.set(path.clone());
    }
    let confirm = gtk4::Button::with_label("Read this machine's local file");
    confirm.set_sensitive(path.is_absolute());
    let prepare = gtk4::Button::with_label("Prepare this local target");
    prepare.set_visible(!path.is_absolute());
    let prepare_path = path.clone();
    let prepare_target = prepared.clone();
    let prepare_directory = directory.clone();
    let prepare_confirm = confirm.downgrade();
    let prepare_label = resolved.downgrade();
    prepare.connect_clicked(move |button| {
        if prepare_target.get().is_some() {
            return;
        }
        match local_preview_file::selected_local_path(
            &prepare_path,
            prepare_directory.text().as_str(),
        ) {
            Ok(target) => {
                if let Some(label) = prepare_label.upgrade() {
                    label.set_text(&format!("Prepared local file: {}", target.display()));
                }
                if prepare_target.set(target).is_ok() {
                    prepare_directory.set_sensitive(false);
                    button.set_sensitive(false);
                    if let Some(confirm) = prepare_confirm.upgrade() {
                        confirm.set_sensitive(true);
                    }
                }
            }
            Err(_) => {
                if let Some(label) = prepare_label.upgrade() {
                    label.set_text("Enter an absolute local directory without parent components");
                }
            }
        }
    });
    let close = gtk4::Button::with_label("Close preview");
    text.set_editable(false);
    text.set_cursor_visible(false);
    text.set_monospace(true);
    text.set_wrap_mode(gtk4::WrapMode::None);
    let scroll = gtk4::ScrolledWindow::new();
    scroll.set_hexpand(true);
    scroll.set_vexpand(true);
    scroll.set_min_content_width(1);
    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    content.append(&text);
    content.append(&picture);
    scroll.set_child(Some(&content));
    for widget in [
        title.upcast::<gtk4::Widget>(),
        path_label.upcast(),
        warning.upcast(),
        directory.clone().upcast(),
        resolved.upcast(),
        prepare.upcast(),
        confirm.clone().upcast(),
        close.clone().upcast(),
        scroll.upcast(),
    ] {
        panel.append(&widget);
    }
    let weak_stack = ui.side_stack.downgrade();
    let close_state = state.clone();
    close.connect_clicked(move |_| {
        if close_state.generation.get() != token {
            return;
        }
        close_state.generation.set(token.wrapping_add(1));
        if let Some(stack) = weak_stack.upgrade() {
            let previous = close_state.previous.borrow();
            if stack.child_by_name(&previous).is_some() {
                stack.set_visible_child_name(&previous);
            }
            if let Some(old) = stack.child_by_name(PAGE) {
                stack.remove(&old);
            }
        }
        if let Some(restore) = close_state.restore.borrow_mut().take() {
            restore();
        }
    });
    let state = state.clone();
    let buffer = text.buffer().downgrade();
    confirm.connect_clicked(move |button| {
        if state.generation.get() != token { return; }
        let Some(buffer) = buffer.upgrade() else { return; };
        let Some(path) = prepared.get().cloned() else {
            buffer.set_text("Prepare the complete local target before confirming"); return;
        };
        if READER_BUSY.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed).is_err() {
            buffer.set_text("Another local preview reader is still busy"); return;
        }
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        button.set_sensitive(false);
        picture.set_paintable(None::<&gtk4::gdk::Paintable>);
        picture.set_visible(false);
        buffer.set_text("Reading bounded local preview…");
        if std::thread::Builder::new().name("local-source-preview".into()).spawn(move || {
            struct Release;
            impl Drop for Release { fn drop(&mut self) { READER_BUSY.store(false, Ordering::Release); } }
            let _release = Release;
            let result = crate::local_preview_content::read_confirmed(&path).map_err(|_| "Cannot preview: require a regular local UTF-8 source (256 KiB) or bounded PNG/JPEG (4 MiB, 4096-pixel edges, 4M pixels), with no symlink or parent components.".to_owned());
            let _ = tx.send(result);
        }).is_err() {
            READER_BUSY.store(false, Ordering::Release);
            button.set_sensitive(true);
            buffer.set_text("Could not start preview reader"); return;
        }
        let weak_picture = picture.downgrade();
        let weak_button = button.downgrade();
        let weak_buffer = buffer.downgrade();
        let state = state.clone();
        gtk4::glib::timeout_add_local(Duration::from_millis(40), move || {
            if state.generation.get() != token || weak_buffer.upgrade().is_none() { return gtk4::glib::ControlFlow::Break; }
            match rx.try_recv() {
                Err(std::sync::mpsc::TryRecvError::Empty) => gtk4::glib::ControlFlow::Continue,
                result => {
                    if let Some(button) = weak_button.upgrade() { button.set_sensitive(true); }
                    if let Some(buffer) = weak_buffer.upgrade() {
                        match result {
                            Ok(Ok(crate::local_preview_content::Content::Text(text))) => buffer.set_text(&text),
                            Ok(Ok(crate::local_preview_content::Content::Image(image))) => {
                                if let Some(picture) = weak_picture.upgrade() {
                                    let bytes = gtk4::glib::Bytes::from_owned(image.pixels);
                                    let texture = gtk4::gdk::MemoryTexture::new(
                                        image.width as i32, image.height as i32,
                                        gtk4::gdk::MemoryFormat::R8g8b8a8, &bytes, image.width as usize * 4);
                                    picture.set_paintable(Some(&texture));
                                    picture.set_visible(true);
                                    buffer.set_text(&format!("Read-only image: {} × {} pixels", image.width, image.height));
                                }
                            }
                            Ok(Err(error)) => buffer.set_text(&error),
                            Err(_) => buffer.set_text("Preview reader stopped"),
                        }
                    }
                    gtk4::glib::ControlFlow::Break
                }
            }
        });
    });
    ui.side_stack.add_named(&panel, Some(PAGE));
    ui.side_stack.set_visible_child_name(PAGE);
    ui.side_stack.set_visible(true);
    (ui.show)();
}

pub(crate) fn install(
    window: &gtk4::Widget,
    stack: &gtk4::Stack,
    show: impl Fn() + 'static,
    capture: impl Fn() -> Box<dyn Fn()> + 'static,
) {
    let ui = Rc::new(PreviewUi {
        side_stack: stack.clone(),
        show: Box::new(show),
        capture: Box::new(capture),
    });
    let state = Rc::new(PreviewState::default());
    let weak_window = window.downgrade();
    crate::vte_reference::click(
        window,
        move |x, y| hit(&weak_window.upgrade()?, x, y),
        move |(_, path)| show_candidate(&ui, &state, path),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn candidates_are_not_remote_uris_or_relative_cwd_guesses() {
        for name in ["README.md", "package.json", "App.tsx:12:3"] {
            assert_eq!(
                candidate(name),
                Some(PathBuf::from(name)),
                "exact filename is preserved"
            );
        }
        for rejected in ["ordinary", "example.com", "README.md:notes", "mailto:a.md"] {
            assert!(candidate(rejected).is_none());
        }
        assert_eq!(candidate("src/App.tsx"), Some(PathBuf::from("src/App.tsx")));
        assert_eq!(candidate("./README.md"), Some(PathBuf::from("./README.md")));
        assert!(candidate("src/../secrets").is_none());
        assert!(candidate("~/notes.md").is_none());
        assert!(candidate("$HOME/src/App.tsx").is_none());
        assert!(candidate(r"C:\src/App.tsx").is_none());
        assert_eq!(
            candidate("日本語/src/App.tsx"),
            Some(PathBuf::from("日本語/src/App.tsx"))
        );
        assert_eq!(
            candidate("file:///tmp/source.tsx").unwrap(),
            PathBuf::from("/tmp/source.tsx")
        );
        assert_eq!(
            candidate("file://localhost/tmp/source%20file.md").unwrap(),
            PathBuf::from("/tmp/source file.md")
        );
        for value in [
            "file://remote/tmp/a.md",
            "https://example.com/a.md",
            "../a.md",
            "javascript:alert(1)",
            "file:///tmp/a%0ab.md",
        ] {
            assert!(candidate(value).is_none(), "{value}");
        }
    }
}
