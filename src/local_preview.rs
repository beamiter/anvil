//! Confirmation-first, nonexecuting local source preview. Unknown live VTE
//! ownership remains with the application; source text never opens an editor.
use crate::local_preview_content::{
    self as reader, read_confirmed as read_exact, Content as PreviewContent,
    LocatedContent as LoadedPreview,
};
use crate::{local_preview_file, local_reference};
use gtk4::prelude::*;
use relm4::gtk as gtk4;
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use vte4::{prelude::TerminalExtManual, TerminalExt};

static READER_BUSY: AtomicBool = AtomicBool::new(false);
const PAGE: &str = "local-source-preview";
#[derive(Default)]
pub(crate) struct PreviewState {
    current: RefCell<Option<Rc<PreviewPanel>>>,
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
    let explicit = terminal.check_hyperlink_at(point.x().into(), point.y().into());
    let reference = if explicit.is_none() {
        crate::vte_reference::hit(&terminal, point.x().into(), point.y().into())
    } else {
        None
    };
    if reference
        .as_ref()
        .is_some_and(|reference| reference.blocked)
    {
        return None;
    }
    let raw = explicit
        .map(|s| s.to_string())
        .or_else(|| reference.map(|reference| reference.target))
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

#[derive(Clone)]
struct ReadTarget {
    path: PathBuf,
    source_reference: bool,
}

struct PreviewPanel {
    root: gtk4::Box,
    path: gtk4::Label,
    approval: gtk4::ScrolledWindow,
    disclosure: gtk4::Label,
    directory: gtk4::Entry,
    prepare: gtk4::Button,
    confirm: gtk4::Button,
    status: gtk4::Label,
    tools: gtk4::FlowBox,
    reload: gtk4::Button,
    copy_path: gtk4::Button,
    copy_text: gtk4::Button,
    source_tools: gtk4::Box,
    line: gtk4::Entry,
    actual_size: gtk4::ToggleButton,
    body: gtk4::Stack,
    text: gtk4::TextView,
    highlight: gtk4::TextTag,
    picture: gtk4::Picture,
    image_scroll: gtk4::ScrolledWindow,
    target: RefCell<Option<ReadTarget>>,
    active: Cell<bool>,
    loaded: Cell<bool>,
    loading: Cell<bool>,
    seek: Cell<Option<(u32, Option<u32>)>>,
    pending_scroll: RefCell<Option<gtk4::TickCallbackId>>,
}

fn wrapped_label(text: &str) -> gtk4::Label {
    gtk4::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk4::pango::WrapMode::WordChar)
        .max_width_chars(36)
        .build()
}

fn scroll_body() -> gtk4::ScrolledWindow {
    gtk4::ScrolledWindow::builder()
        .hexpand(true)
        .vexpand(true)
        .min_content_width(1)
        .min_content_height(1)
        .build()
}

impl PreviewPanel {
    fn new(
        path: PathBuf,
        from_terminal: bool,
        close_preview: impl Fn(&Rc<Self>) + 'static,
    ) -> Rc<Self> {
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        root.set_margin_top(8);
        root.set_margin_bottom(8);
        root.set_margin_start(8);
        root.set_margin_end(8);

        let heading = gtk4::Label::builder()
            .label("Local file preview")
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .build();
        heading.add_css_class("heading");
        let close = gtk4::Button::with_label("Close");
        let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        header.append(&heading);
        header.append(&close);
        let path_label = gtk4::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk4::pango::EllipsizeMode::Middle)
            .selectable(true)
            .build();

        let relative = !path.is_absolute();
        let approval = scroll_body();
        approval.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        let prompt = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        prompt.append(&wrapped_label(
            "Confirm that the terminal reference belongs to this computer before reading.",
        ));
        let directory = gtk4::Entry::builder()
            .placeholder_text("Absolute local directory")
            .tooltip_text("Type the directory on this computer.")
            .max_length(4096)
            .width_chars(1)
            .visible(relative)
            .build();
        let prepare = gtk4::Button::with_label("Prepare local target");
        prepare.set_visible(relative);
        prepare.set_sensitive(false);
        let disclosure = wrapped_label("");
        disclosure.set_selectable(true);
        let confirm = gtk4::Button::with_label("Read local file");
        confirm.set_sensitive(!relative);
        prompt.append(&directory);
        prompt.append(&prepare);
        prompt.append(&disclosure);
        prompt.append(&confirm);
        approval.set_child(Some(&prompt));
        approval.set_visible(from_terminal);

        let tools = gtk4::FlowBox::builder()
            .min_children_per_line(1)
            .max_children_per_line(3)
            .column_spacing(4)
            .row_spacing(4)
            .selection_mode(gtk4::SelectionMode::None)
            .build();
        let reload = gtk4::Button::with_label("Reload");
        let copy_path = gtk4::Button::with_label("Copy path");
        let copy_text = gtk4::Button::with_label("Copy text");
        copy_text.set_visible(false);
        copy_path.set_sensitive(false);
        tools.append(&reload);
        tools.append(&copy_path);
        tools.append(&copy_text);
        tools.set_visible(!from_terminal);

        let source_tools = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        let line = gtk4::Entry::builder()
            .placeholder_text("Line number")
            .tooltip_text("One-based source line")
            .input_purpose(gtk4::InputPurpose::Digits)
            .max_length(10)
            .width_chars(8)
            .hexpand(true)
            .build();
        let jump = gtk4::Button::with_label("Go to line");
        source_tools.append(&line);
        source_tools.append(&jump);
        source_tools.set_visible(false);
        let actual_size = gtk4::ToggleButton::with_label("Actual size");
        actual_size.set_halign(gtk4::Align::Start);
        actual_size.set_visible(false);
        let status = wrapped_label("");
        status.add_css_class("dim-label");
        status.set_visible(false);

        let text = gtk4::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .wrap_mode(gtk4::WrapMode::None)
            .left_margin(6)
            .right_margin(6)
            .build();
        let highlight = gtk4::TextTag::builder()
            .underline(gtk4::pango::Underline::Single)
            .build();
        text.buffer().tag_table().add(&highlight);
        let source_scroll = scroll_body();
        source_scroll.set_child(Some(&text));
        let picture = gtk4::Picture::builder()
            .can_shrink(true)
            .content_fit(gtk4::ContentFit::ScaleDown)
            .build();
        let image_scroll = scroll_body();
        image_scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Never);
        image_scroll.set_child(Some(&picture));
        let body = gtk4::Stack::builder().vexpand(true).visible(false).build();
        body.add_named(&source_scroll, Some("source"));
        body.add_named(&image_scroll, Some("image"));

        root.append(&header);
        root.append(&path_label);
        root.append(&status);
        root.append(&approval);
        root.append(&tools);
        root.append(&source_tools);
        root.append(&actual_size);
        root.append(&body);

        let panel = Rc::new(Self {
            root,
            path: path_label,
            approval,
            disclosure,
            directory,
            prepare,
            confirm,
            status,
            tools,
            reload,
            copy_path,
            copy_text,
            source_tools,
            line,
            actual_size,
            body,
            text,
            highlight,
            picture,
            image_scroll,
            target: RefCell::new((!relative).then(|| ReadTarget {
                path: path.clone(),
                source_reference: from_terminal,
            })),
            active: Cell::new(true),
            loaded: Cell::new(false),
            loading: Cell::new(false),
            seek: Cell::new(None),
            pending_scroll: RefCell::new(None),
        });
        panel.show_path(&path);
        if from_terminal && !relative {
            panel
                .disclosure
                .set_text(&reader::confirmation_target(&path));
        }
        if relative {
            let weak = Rc::downgrade(&panel);
            let reference = path.clone();
            panel.directory.connect_changed(move |entry| {
                if let Some(panel) = weak.upgrade() {
                    let prepared =
                        local_preview_file::selected_local_path(&reference, entry.text().as_str());
                    panel.prepare.set_sensitive(prepared.is_ok());
                    if let Ok(prepared) = prepared {
                        panel
                            .disclosure
                            .set_text(&reader::confirmation_target(&prepared));
                    } else {
                        panel.disclosure.set_text("");
                    }
                }
            });
            let weak = Rc::downgrade(&panel);
            panel.prepare.connect_clicked(move |_| {
                let Some(panel) = weak.upgrade() else {
                    return;
                };
                if panel.target.borrow().is_none() {
                    if let Ok(target) = local_preview_file::selected_local_path(
                        &path,
                        panel.directory.text().as_str(),
                    ) {
                        panel
                            .disclosure
                            .set_text(&reader::confirmation_target(&target));
                        panel.target.replace(Some(ReadTarget {
                            path: target,
                            source_reference: true,
                        }));
                        panel.directory.set_editable(false);
                        panel.prepare.set_sensitive(false);
                        panel.confirm.set_sensitive(true);
                    }
                }
            });
        }

        let close_preview = Rc::new(close_preview);
        let close_click = close_preview.clone();
        let weak = Rc::downgrade(&panel);
        close.connect_clicked(move |_| {
            if let Some(panel) = weak.upgrade() {
                close_click(&panel);
            }
        });
        let weak = Rc::downgrade(&panel);
        let keys = gtk4::EventControllerKey::new();
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gtk4::gdk::Key::Escape {
                if let Some(panel) = weak.upgrade() {
                    close_preview(&panel);
                    return gtk4::glib::Propagation::Stop;
                }
            }
            gtk4::glib::Propagation::Proceed
        });
        panel.root.add_controller(keys);
        for button in [&panel.confirm, &panel.reload] {
            let weak = Rc::downgrade(&panel);
            button.connect_clicked(move |_| {
                if let Some(panel) = weak.upgrade() {
                    panel.read();
                }
            });
        }
        let weak = Rc::downgrade(&panel);
        jump.connect_clicked(move |_| {
            if let Some(panel) = weak.upgrade() {
                panel.jump_from_entry();
            }
        });
        let weak = Rc::downgrade(&panel);
        panel.line.connect_activate(move |_| {
            if let Some(panel) = weak.upgrade() {
                panel.jump_from_entry();
            }
        });
        let weak = Rc::downgrade(&panel);
        panel.copy_path.connect_clicked(move |button| {
            if let Some(panel) = weak.upgrade().filter(|panel| panel.loaded.get()) {
                button.clipboard().set_text(panel.path.text().as_str());
            }
        });
        let weak = Rc::downgrade(&panel);
        panel.copy_text.connect_clicked(move |button| {
            if let Some(panel) = weak
                .upgrade()
                .filter(|panel| panel.source_tools.is_visible())
            {
                let buffer = panel.text.buffer();
                button.clipboard().set_text(
                    buffer
                        .text(&buffer.start_iter(), &buffer.end_iter(), true)
                        .as_str(),
                );
            }
        });
        let weak = Rc::downgrade(&panel);
        panel.actual_size.connect_toggled(move |_| {
            if let Some(panel) = weak.upgrade() {
                panel.resize_image();
            }
        });
        panel
    }

    fn show_path(&self, path: &Path) {
        let value = display_path(path);
        self.path.set_text(&value);
        self.path.set_tooltip_text(Some(&value));
    }

    fn show_status(&self, value: &str) {
        self.status.set_text(value);
        self.status.set_visible(!value.is_empty());
    }

    fn read(self: &Rc<Self>) {
        if !self.active.get() || self.loading.get() {
            return;
        }
        let Some(target) = self.target.borrow().clone() else {
            return;
        };
        if READER_BUSY
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            self.show_status("Another preview is still reading. Try again shortly.");
            return;
        }
        self.cancel_scroll();
        self.loading.set(true);
        self.loaded.set(false);
        self.approval.set_visible(false);
        self.tools.set_visible(true);
        self.reload.set_sensitive(false);
        self.copy_path.set_sensitive(false);
        self.copy_text.set_visible(false);
        self.source_tools.set_visible(false);
        self.actual_size.set_visible(false);
        self.body.set_visible(false);
        self.text.buffer().set_text("");
        self.picture.set_paintable(None::<&gtk4::gdk::Texture>);
        self.show_status("Reading local file…");

        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        if std::thread::Builder::new()
            .name("local-file-preview".into())
            .spawn(move || {
                struct ReleaseReader;
                impl Drop for ReleaseReader {
                    fn drop(&mut self) {
                        READER_BUSY.store(false, Ordering::Release);
                    }
                }
                let _reader = ReleaseReader;
                let result = if target.source_reference {
                    reader::read_reference(&target.path)
                } else {
                    read_exact(&target.path).map(|content| LoadedPreview {
                        path: target.path,
                        content,
                        location: None,
                    })
                };
                let _ = tx.send(result);
            })
            .is_err()
        {
            READER_BUSY.store(false, Ordering::Release);
            self.finish(Err(std::io::Error::other(
                "Could not start the preview reader",
            )));
            return;
        }

        let weak = Rc::downgrade(self);
        gtk4::glib::timeout_add_local(Duration::from_millis(40), move || {
            let Some(panel) = weak.upgrade().filter(|panel| panel.active.get()) else {
                return gtk4::glib::ControlFlow::Break;
            };
            let result = match rx.try_recv() {
                Ok(result) => result,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    return gtk4::glib::ControlFlow::Continue
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Err(std::io::Error::other("Preview reader stopped"))
                }
            };
            panel.finish(result);
            gtk4::glib::ControlFlow::Break
        });
    }

    fn finish(&self, result: std::io::Result<LoadedPreview>) {
        self.loading.set(false);
        self.reload.set_sensitive(true);
        let preview = match result {
            Ok(preview) => preview,
            Err(error) => {
                let description = match error.kind() {
                    std::io::ErrorKind::NotFound => "Local file not found".into(),
                    std::io::ErrorKind::PermissionDenied => {
                        "Permission denied reading the local file".into()
                    }
                    _ => error.to_string(),
                };
                self.show_status(&description);
                return;
            }
        };
        self.show_path(&preview.path);
        self.loaded.set(true);
        self.copy_path.set_sensitive(true);
        self.target.replace(Some(ReadTarget {
            path: preview.path,
            source_reference: false,
        }));
        self.body.set_visible(true);
        match preview.content {
            PreviewContent::Text(source) => {
                self.body.set_visible_child_name("source");
                self.text.buffer().set_text(&source);
                self.copy_text.set_visible(!source.is_empty());
                self.source_tools.set_visible(!source.is_empty());
                let target = preview
                    .location
                    .map(|location| {
                        (
                            location.line.get(),
                            location.column.map(|column| column.get()),
                        )
                    })
                    .or(self.seek.get());
                if let Some((line, column)) = target {
                    self.jump_to(line, column);
                } else if source.is_empty() {
                    self.show_status("Empty file");
                } else {
                    self.show_status(&format!("{} lines", self.text.buffer().line_count()));
                }
            }
            PreviewContent::Image(image) => {
                self.body.set_visible_child_name("image");
                let bytes = gtk4::glib::Bytes::from_owned(image.pixels);
                let texture = gtk4::gdk::MemoryTexture::new(
                    image.width as i32,
                    image.height as i32,
                    gtk4::gdk::MemoryFormat::R8g8b8a8,
                    &bytes,
                    image.width as usize * 4,
                );
                self.picture.set_paintable(Some(&texture));
                self.actual_size.set_visible(true);
                self.resize_image();
                self.show_status(&format!("{} × {} pixels", image.width, image.height));
            }
        }
    }

    fn jump_from_entry(&self) {
        if !self.source_tools.is_visible() {
            return;
        }
        match self.line.text().trim().parse() {
            Ok(line) => self.jump_to(line, None),
            Err(_) => self.show_status("Enter a line number."),
        }
    }

    fn jump_to(&self, line: u32, column: Option<u32>) {
        let buffer = self.text.buffer();
        let count = buffer.line_count() as u32;
        let location = match column {
            Some(column) => format!("Line {line}, column {column} (reported)"),
            None => format!("Line {line}"),
        };
        if !(1..=count).contains(&line) {
            self.show_status(&format!(
                "{location} is outside this file. Choose 1–{count}."
            ));
            return;
        }

        if let Some(start) = buffer.iter_at_line((line - 1) as i32) {
            buffer.remove_tag(&self.highlight, &buffer.start_iter(), &buffer.end_iter());
            let mut end = start;
            end.forward_to_line_end();
            buffer.apply_tag(&self.highlight, &start, &end);
            self.scroll_to_line(start.line());
            self.seek.set(Some((line, column)));
            self.line.set_text(&line.to_string());
            self.show_status(&format!("{location} · {count} lines"));
        }
    }

    fn cancel_scroll(&self) {
        if let Some(callback) = self.pending_scroll.borrow_mut().take() {
            callback.remove();
        }
    }

    fn scroll_to_line(&self, line: i32) {
        self.cancel_scroll();
        // The adjustment describes a partially validated layout just after a
        // buffer replacement. Wait for the target row to have real geometry.
        let ticks = Cell::new(0);
        let callback = self.text.add_tick_callback(move |view, _| {
            let Some(scroll) = view.vadjustment() else {
                return gtk4::glib::ControlFlow::Break;
            };
            let Some(iter) = view.buffer().iter_at_line(line) else {
                return gtk4::glib::ControlFlow::Break;
            };
            let (y, height) = view.line_yrange(&iter);
            if scroll.page_size() <= 1.0 || height <= 0 || f64::from(y + height) > scroll.upper() {
                ticks.set(ticks.get() + 1);
                return if ticks.get() < 30 {
                    gtk4::glib::ControlFlow::Continue
                } else {
                    gtk4::glib::ControlFlow::Break
                };
            }
            let limit = (scroll.upper() - scroll.page_size()).max(0.0);
            scroll.set_value((f64::from(y) - scroll.page_size() * 0.12).clamp(0.0, limit));
            if let Some(horizontal) = view.hadjustment() {
                horizontal.set_value(0.0);
            }
            gtk4::glib::ControlFlow::Break
        });
        self.pending_scroll.replace(Some(callback));
    }

    fn resize_image(&self) {
        let actual = self.actual_size.is_active();
        let policy = if actual {
            gtk4::PolicyType::Automatic
        } else {
            gtk4::PolicyType::Never
        };
        self.image_scroll.set_policy(policy, policy);
        self.picture.set_can_shrink(!actual);
        self.image_scroll.hadjustment().set_value(0.0);
        self.image_scroll.vadjustment().set_value(0.0);
    }
}

fn display_path(path: &Path) -> String {
    crate::file_tree::display_full_path(path)
}

impl PreviewState {
    fn observe_stack(self: &Rc<Self>, stack: &gtk4::Stack) {
        let state = Rc::downgrade(self);
        stack.connect_visible_child_name_notify(move |stack| {
            if stack.visible_child_name().as_deref() == Some(PAGE) {
                return;
            }
            if let Some(state) = state.upgrade() {
                let retired = state.current.borrow_mut().take();
                if let Some(retired) = retired {
                    state.restore.borrow_mut().take();
                    retired.active.set(false);
                    retired.cancel_scroll();
                    stack.remove(&retired.root);
                }
            }
        });
    }
}

impl PreviewState {
    fn present(self: &Rc<Self>, stack: &gtk4::Stack, path: PathBuf, from_terminal: bool) {
        if stack.visible_child_name().as_deref() != Some(PAGE) {
            *self.previous.borrow_mut() = stack
                .visible_child_name()
                .map(|name| name.to_string())
                .unwrap_or_else(|| "overview".into());
        }
        let retired = self.current.borrow_mut().take();
        if let Some(retired) = retired {
            retired.active.set(false);
            retired.cancel_scroll();
            stack.remove(&retired.root);
        }
        let weak_state = Rc::downgrade(self);
        let weak_stack = stack.downgrade();
        let panel = PreviewPanel::new(path, from_terminal, move |panel| {
            if let (Some(state), Some(stack)) = (weak_state.upgrade(), weak_stack.upgrade()) {
                let still_current = state
                    .current
                    .borrow()
                    .as_ref()
                    .is_some_and(|current| Rc::ptr_eq(current, panel));
                if still_current {
                    state.close(&stack);
                }
            }
        });
        stack.add_named(&panel.root, Some(PAGE));
        self.current.replace(Some(panel.clone()));
        stack.set_visible_child_name(PAGE);
        if !from_terminal {
            panel.read();
        }
    }

    fn close(&self, stack: &gtk4::Stack) {
        let retired = self.current.borrow_mut().take();
        if let Some(retired) = retired {
            let previous = self.previous.borrow();
            if stack.child_by_name(&previous).is_some() {
                stack.set_visible_child_name(&previous);
            }
            retired.active.set(false);
            retired.cancel_scroll();
            stack.remove(&retired.root);
        }
        if let Some(restore) = self.restore.borrow_mut().take() {
            restore();
        }
    }
}

fn show_candidate(
    ui: &Rc<PreviewUi>,
    state: &Rc<PreviewState>,
    path: PathBuf,
    from_terminal: bool,
) {
    if state.restore.borrow().is_none() {
        *state.restore.borrow_mut() = Some((ui.capture)());
    }
    state.present(&ui.side_stack, path, from_terminal);
    ui.side_stack.set_visible(true);
    (ui.show)();
}

pub(crate) fn install(
    window: &gtk4::Widget,
    stack: &gtk4::Stack,
    show: impl Fn() + 'static,
    capture: impl Fn() -> Box<dyn Fn()> + 'static,
) -> Box<dyn Fn(PathBuf)> {
    let ui = Rc::new(PreviewUi {
        side_stack: stack.clone(),
        show: Box::new(show),
        capture: Box::new(capture),
    });
    let state = Rc::new(PreviewState::default());
    state.observe_stack(stack);
    let weak_window = window.downgrade();
    let click_ui = ui.clone();
    let click_state = state.clone();
    crate::vte_reference::click(
        window,
        move |x, y| hit(&weak_window.upgrade()?, x, y),
        move |(_, path)| show_candidate(&click_ui, &click_state, path, true),
    );
    Box::new(move |path| show_candidate(&ui, &state, path, false))
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
    #[test]
    #[ignore = "requires a GTK display"]
    fn mapped_preview_jumps_reloads_and_retires_the_page() {
        gtk4::init().expect("GTK display");
        let fixture =
            std::env::temp_dir().join(format!("sidebar-preview-mapped-{}", std::process::id()));
        std::fs::create_dir(&fixture).unwrap();
        let path = fixture.join("source.rs");
        let source = (1..=1600)
            .map(|line| format!("row {line}\n"))
            .collect::<String>();
        std::fs::write(&path, source).unwrap();

        let stack = gtk4::Stack::new();
        stack.add_named(&gtk4::Label::new(Some("Files")), Some("files"));
        let state = Rc::new(PreviewState::default());
        state.observe_stack(&stack);
        let window = gtk4::Window::builder()
            .child(&stack)
            .default_width(280)
            .default_height(410)
            .build();
        window.present();
        let wait = |stage: &str, condition: &dyn Fn() -> bool| {
            for _ in 0..400 {
                while gtk4::glib::MainContext::default().iteration(false) {}
                if condition() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("preview did not settle during {stage}");
        };

        let reference = fixture.join("source.rs:1400:3");
        state.present(&stack, reference.clone(), true);
        let panel = state.current.borrow().as_ref().unwrap().clone();
        assert!(panel.approval.is_visible() && !panel.loading.get());
        panel.confirm.emit_clicked();
        let target_visible = || {
            let Some(scroll) = panel.text.vadjustment() else {
                return false;
            };
            let Some(iter) = panel.text.buffer().iter_at_line(1399) else {
                return false;
            };
            let (y, height) = panel.text.line_yrange(&iter);
            panel.loaded.get()
                && scroll.value() > 1000.0
                && f64::from(y) >= scroll.value()
                && f64::from(y + height) <= scroll.value() + scroll.page_size()
        };
        wait("source navigation", &target_visible);
        assert_eq!(panel.line.text(), "1400");
        assert!(panel.status.text().contains("column 3"));
        assert_eq!(panel.target.borrow().as_ref().unwrap().path, path);

        std::fs::write(&reference, "literal colon filename").unwrap();
        panel.reload.emit_clicked();
        wait("source reload", &target_visible);
        assert_eq!(panel.target.borrow().as_ref().unwrap().path, path);
        assert_eq!(panel.text.buffer().line_count(), 1601);

        state.present(&stack, reference, false);
        let exact = state.current.borrow().as_ref().unwrap().clone();
        wait("exact filename read", &|| exact.loaded.get());
        assert_eq!(exact.text.buffer().line_count(), 1);
        assert_eq!(exact.seek.get(), None);
        assert!(!panel.active.get());

        let image_path = fixture.join("sample.png");
        image::RgbaImage::from_pixel(640, 480, image::Rgba([24, 80, 130, 255]))
            .save(&image_path)
            .unwrap();
        state.present(&stack, image_path, false);
        let image = state.current.borrow().as_ref().unwrap().clone();
        wait("image read", &|| image.loaded.get());
        assert_eq!(image.status.text(), "640 × 480 pixels");
        assert_eq!(
            image.image_scroll.hscrollbar_policy(),
            gtk4::PolicyType::Never
        );
        image.actual_size.set_active(true);
        wait("actual size", &|| {
            let scroll = image.image_scroll.hadjustment();
            scroll.upper() > scroll.page_size() + 100.0
        });
        assert_eq!(window.width(), 280);
        state.close(&stack);
        assert!(!image.active.get());
        assert_eq!(stack.visible_child_name().as_deref(), Some("files"));
        assert!(stack.child_by_name(PAGE).is_none());
        window.close();
        std::fs::remove_dir_all(fixture).unwrap();
    }
}
