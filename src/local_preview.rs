//! Confirmation-first, nonexecuting local source preview. Unknown live VTE
//! ownership remains with the application; source text never opens an editor.
use crate::local_preview_content::{
    self as reader, read_confirmed as read_exact, Content as PreviewContent,
    LocatedContent as LoadedPreview,
};
use crate::preview_navigation::{match_background, ImageZoom, Occurrence, SourceMatches};
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
    read_status: gtk4::Label,
    tools: gtk4::FlowBox,
    reload: gtk4::Button,
    copy_path: gtk4::Button,
    copy_text: gtk4::Button,
    copy_line: gtk4::Button,
    source_tools: gtk4::Box,
    line: gtk4::Entry,
    find: gtk4::ToggleButton,
    find_tools: gtk4::Box,
    find_query: gtk4::Entry,
    match_case: gtk4::CheckButton,
    find_count: gtk4::Label,
    find_previous: gtk4::Button,
    find_next: gtk4::Button,
    matches: RefCell<SourceMatches>,
    zoom: gtk4::DropDown,
    texture: RefCell<Option<gtk4::gdk::Texture>>,
    body: gtk4::Stack,
    text: gtk4::TextView,
    highlight: gtk4::TextTag,
    match_tag: gtk4::TextTag,
    active_match_tag: gtk4::TextTag,
    picture: gtk4::Picture,
    image_scroll: gtk4::ScrolledWindow,
    target: RefCell<Option<ReadTarget>>,
    active: Cell<bool>,
    loaded: Cell<bool>,
    loading: Cell<bool>,
    seek: Cell<Option<(u32, Option<u32>)>>,
    pending_scroll: RefCell<Option<gtk4::TickCallbackId>>,
}

enum HorizontalTarget {
    Offset(f64),
    MatchStart(usize),
}

fn match_tag(active: bool) -> gtk4::TextTag {
    let [red, green, blue] = match_background(active).map(|channel| f32::from(channel) / 255.0);
    gtk4::TextTag::builder()
        .background_rgba(&gtk4::gdk::RGBA::new(red, green, blue, 1.0))
        .foreground_rgba(&gtk4::gdk::RGBA::new(0.14, 0.09, 0.03, 1.0))
        .build()
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
        let copy_line = gtk4::Button::with_label("Copy line");
        copy_line.set_tooltip_text(Some("Copy the highlighted line without its line number"));
        copy_line.set_visible(false);
        copy_line.set_sensitive(false);
        copy_path.set_sensitive(false);
        tools.append(&reload);
        tools.append(&copy_path);
        tools.append(&copy_text);
        tools.append(&copy_line);
        tools.set_visible(!from_terminal);

        let source_tools = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        let line = gtk4::Entry::builder()
            .placeholder_text("Line")
            .tooltip_text("One-based source line")
            .input_purpose(gtk4::InputPurpose::Digits)
            .max_length(10)
            .width_chars(4)
            .hexpand(true)
            .build();
        let jump = gtk4::Button::with_label("Go to line");
        source_tools.append(&line);
        source_tools.append(&jump);
        let find = gtk4::ToggleButton::with_label("Find");
        source_tools.append(&find);
        source_tools.set_visible(false);

        let find_query = gtk4::Entry::builder()
            .placeholder_text("Find text")
            .tooltip_text("Search for literal text in this file")
            .width_chars(1)
            .max_length(256)
            .hexpand(true)
            .build();
        let find_previous = gtk4::Button::with_label("Prev");
        find_previous.set_tooltip_text(Some("Previous match"));
        let find_next = gtk4::Button::with_label("Next");
        find_next.set_tooltip_text(Some("Next match"));
        let match_case = gtk4::CheckButton::with_label("Match case");
        match_case.set_active(true);
        let search_row = gtk4::FlowBox::builder()
            .min_children_per_line(1)
            .max_children_per_line(3)
            .column_spacing(4)
            .row_spacing(4)
            .selection_mode(gtk4::SelectionMode::None)
            .build();
        search_row.append(&find_previous);
        search_row.append(&find_next);
        search_row.append(&match_case);
        let find_count = wrapped_label("");
        find_count.add_css_class("dim-label");
        let find_tools = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        find_tools.append(&find_query);
        find_tools.append(&search_row);
        find_tools.append(&find_count);
        find_tools.set_visible(false);
        let zoom_labels = ImageZoom::OPTIONS.map(|zoom| zoom.to_string());
        let zoom = gtk4::DropDown::from_strings(&zoom_labels.each_ref().map(String::as_str));
        zoom.set_tooltip_text(Some("Image zoom"));
        zoom.set_halign(gtk4::Align::Start);
        zoom.set_visible(false);
        let status = wrapped_label("");
        status.add_css_class("dim-label");
        status.set_visible(false);
        let read_status = wrapped_label("");
        read_status.set_visible(false);

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
        let table = text.buffer().tag_table();
        let active_match_tag = match_tag(true);
        let match_tag = match_tag(false);
        for tag in [&highlight, &match_tag, &active_match_tag] {
            table.add(tag);
        }
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
        root.append(&read_status);
        root.append(&approval);
        root.append(&tools);
        root.append(&source_tools);
        root.append(&find_tools);
        root.append(&zoom);
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
            read_status,
            tools,
            reload,
            copy_path,
            copy_text,
            copy_line,
            source_tools,
            line,
            find,
            find_tools,
            find_query,
            match_case,
            find_count,
            find_previous,
            find_next,
            matches: RefCell::new(SourceMatches::default()),
            zoom,
            texture: RefCell::new(None),
            body,
            text,
            highlight,
            match_tag,
            active_match_tag,
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
        panel.find.connect_toggled(move |toggle| {
            if let Some(panel) = weak.upgrade() {
                panel.find_tools.set_visible(toggle.is_active());
                panel.update_match_tags();
                if toggle.is_active() {
                    panel.find_query.grab_focus();
                }
            }
        });
        let weak = Rc::downgrade(&panel);
        panel.find_query.connect_changed(move |_| {
            if let Some(panel) = weak.upgrade() {
                panel.matches.borrow_mut().clear_active();
                panel.update_matches();
            }
        });
        let weak = Rc::downgrade(&panel);
        panel.match_case.connect_toggled(move |_| {
            if let Some(panel) = weak.upgrade() {
                panel.matches.borrow_mut().clear_active();
                panel.update_matches();
            }
        });
        let weak = Rc::downgrade(&panel);
        panel.find_query.connect_activate(move |_| {
            if let Some(panel) = weak.upgrade() {
                panel.find_step(false);
            }
        });
        for (button, previous) in [(&panel.find_next, false), (&panel.find_previous, true)] {
            let weak = Rc::downgrade(&panel);
            button.connect_clicked(move |_| {
                if let Some(panel) = weak.upgrade() {
                    panel.find_step(previous);
                }
            });
        }
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
        panel.copy_line.connect_clicked(move |button| {
            if let Some(line) = weak
                .upgrade()
                .and_then(|panel| panel.selected_source_line())
            {
                button.clipboard().set_text(&line);
            }
        });
        let weak = Rc::downgrade(&panel);
        panel.zoom.connect_selected_notify(move |_| {
            if let Some(panel) = weak.upgrade() {
                panel.resize_image(true);
            }
        });
        let weak = Rc::downgrade(&panel);
        panel.picture.connect_scale_factor_notify(move |_| {
            if let Some(panel) = weak.upgrade() {
                panel.resize_image(false);
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

    fn show_read_status(&self, value: &str) {
        self.read_status.set_text(value);
        self.read_status.set_visible(!value.is_empty());
    }

    fn selected_line(&self) -> Option<usize> {
        self.seek.get().map(|(line, _)| line as usize)
    }

    fn selected_source_line(&self) -> Option<gtk4::glib::GString> {
        if !self.source_tools.is_visible() {
            return None;
        }
        let buffer = self.text.buffer();
        let line = self.selected_line()?.checked_sub(1)?;
        let start = buffer.iter_at_line(line as i32)?;
        let mut end = start;
        if !start.ends_line() {
            end.forward_to_line_end();
        }
        Some(buffer.text(&start, &end, true))
    }

    fn update_matches(&self) {
        let query = self.find_query.text();
        let buffer = self.text.buffer();
        let mut matches = self.matches.borrow_mut();
        if query.is_empty() || !self.source_tools.is_visible() {
            matches.update("", "", true);
        } else {
            let (start, end) = buffer.bounds();
            matches.update(
                buffer.text(&start, &end, true).as_str(),
                query.as_str(),
                self.match_case.is_active(),
            );
        }
        drop(matches);
        self.update_match_tags();
    }

    fn tag_occurrence(&self, buffer: &gtk4::TextBuffer, tag: &gtk4::TextTag, hit: Occurrence) {
        let line = (hit.line - 1) as i32;
        if let (Some(start), Some(end)) = (
            buffer.iter_at_line_index(line, hit.start as i32),
            buffer.iter_at_line_index(line, hit.end as i32),
        ) {
            buffer.apply_tag(tag, &start, &end);
        }
    }

    fn update_match_tags(&self) {
        let buffer = self.text.buffer();
        let (start, end) = buffer.bounds();
        buffer.remove_tag(&self.match_tag, &start, &end);
        if self.find_tools.is_visible() {
            let matches = self.matches.borrow();
            for hit in matches.hits(None) {
                self.tag_occurrence(&buffer, &self.match_tag, *hit);
            }
        }
        self.update_active_match();
    }

    fn update_active_match(&self) {
        let buffer = self.text.buffer();
        let (start, end) = buffer.bounds();
        buffer.remove_tag(&self.active_match_tag, &start, &end);
        let matches = self.matches.borrow();
        if self.find_tools.is_visible() {
            if let Some(hit) = matches.active() {
                self.tag_occurrence(&buffer, &self.active_match_tag, hit);
            }
        }
        let has_query = !self.find_query.text().is_empty();
        self.find_previous.set_sensitive(!matches.is_empty());
        self.find_next.set_sensitive(!matches.is_empty());
        self.find_count.set_visible(has_query);
        if has_query {
            self.find_count.set_text(&matches.status());
        }
    }

    fn find_step(&self, previous: bool) {
        if !self.active.get() || !self.find_tools.is_visible() {
            return;
        }
        let hit = self
            .matches
            .borrow_mut()
            .step(self.selected_line(), previous);
        if let Some(hit) = hit {
            self.jump_to(hit.line as u32, None, false);
            self.update_active_match();
            self.scroll_to_line(
                (hit.line - 1) as i32,
                HorizontalTarget::MatchStart(hit.start),
                None,
            );
        }
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
            self.show_read_status("Another preview is still reading. Try again shortly.");
            return;
        }
        self.loading.set(true);
        self.approval.set_visible(false);
        self.tools.set_visible(true);
        self.reload.set_sensitive(false);
        self.show_read_status(if self.loaded.get() {
            "Reloading local file…"
        } else {
            "Reading local file…"
        });

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
                if self.loaded.get() {
                    self.show_read_status(&format!(
                        "Reload failed. Showing the last preview.\n{description}"
                    ));
                } else {
                    self.reload.set_label("Retry");
                    self.show_read_status(&description);
                }
                return;
            }
        };
        let previous = self
            .loaded
            .get()
            .then(|| self.body.visible_child_name())
            .flatten();
        self.show_read_status("");
        self.reload.set_label("Reload");
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
                let same_kind = previous.as_deref() == Some("source");
                let buffer = self.text.buffer();
                let (start, end) = buffer.bounds();
                let changed = !same_kind || buffer.text(&start, &end, true).as_str() != source;
                let anchor = if same_kind && changed {
                    let y = self.text.visible_rect().y();
                    let (line, top) = self.text.line_at_y(y);
                    let x = self.text.hadjustment().map_or(0.0, |scroll| scroll.value());
                    Some((line.line(), x, f64::from(y - top)))
                } else {
                    None
                };
                self.body.set_visible_child_name("source");
                self.zoom.set_visible(false);
                self.texture.replace(None);
                self.picture.set_paintable(None::<&gtk4::gdk::Texture>);
                self.copy_text.set_visible(!source.is_empty());
                self.copy_line.set_visible(!source.is_empty());
                self.copy_line.set_sensitive(false);
                self.source_tools.set_visible(!source.is_empty());
                self.find_tools
                    .set_visible(!source.is_empty() && self.find.is_active());
                if changed {
                    self.cancel_scroll();
                    buffer.set_text(&source);
                }
                let target = preview
                    .location
                    .map(|location| {
                        (
                            location.line.get(),
                            location.column.map(|column| column.get()),
                        )
                    })
                    .or(self.seek.get());
                if source.is_empty() {
                    self.show_status("Empty file");
                } else if let Some((line, column)) = target {
                    self.jump_to(line, column, !same_kind);
                } else {
                    self.show_status(&format!("{} lines", buffer.line_count()));
                }
                if let Some((line, x, offset)) = anchor.filter(|_| !source.is_empty()) {
                    self.scroll_to_line(
                        line.min(buffer.line_count() - 1),
                        HorizontalTarget::Offset(x),
                        Some(offset),
                    );
                }
                self.update_matches();
            }
            PreviewContent::Image(image) => {
                self.body.set_visible_child_name("image");
                self.cancel_scroll();
                self.text.buffer().set_text("");
                self.copy_text.set_visible(false);
                self.copy_line.set_visible(false);
                self.source_tools.set_visible(false);
                self.find_tools.set_visible(false);
                self.update_matches();
                let bytes = gtk4::glib::Bytes::from_owned(image.pixels);
                let texture = gtk4::gdk::MemoryTexture::new(
                    image.width as i32,
                    image.height as i32,
                    gtk4::gdk::MemoryFormat::R8g8b8a8,
                    &bytes,
                    image.width as usize * 4,
                );
                self.texture.replace(Some(texture.upcast()));
                self.zoom.set_visible(true);
                self.resize_image(previous.as_deref() != Some("image"));
                self.show_status(&format!("{} × {} pixels", image.width, image.height));
            }
        }
    }

    fn jump_from_entry(&self) {
        if !self.source_tools.is_visible() {
            return;
        }
        match self.line.text().trim().parse() {
            Ok(line) => {
                if (1..=self.text.buffer().line_count() as u32).contains(&line) {
                    self.matches.borrow_mut().clear_active();
                    self.update_active_match();
                }
                self.jump_to(line, None, true);
            }
            Err(_) => self.show_status("Enter a line number."),
        }
    }

    fn jump_to(&self, line: u32, column: Option<u32>, reveal: bool) {
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
            if !start.ends_line() {
                end.forward_to_line_end();
                buffer.apply_tag(&self.highlight, &start, &end);
            }
            if reveal {
                self.scroll_to_line(start.line(), HorizontalTarget::Offset(0.0), None);
            }
            self.seek.set(Some((line, column)));
            self.copy_line.set_sensitive(true);
            self.line.set_text(&line.to_string());
            self.show_status(&format!("{location} · {count} lines"));
        }
    }

    fn cancel_scroll(&self) {
        if let Some(callback) = self.pending_scroll.borrow_mut().take() {
            callback.remove();
        }
    }

    fn scroll_to_line(&self, line: i32, target: HorizontalTarget, offset: Option<f64>) {
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
            let offset = offset.unwrap_or_else(|| -scroll.page_size() * 0.12);
            scroll.set_value((f64::from(y) + offset).clamp(0.0, limit));
            if let Some(horizontal) = view.hadjustment() {
                let x = match target {
                    HorizontalTarget::Offset(x) => x,
                    HorizontalTarget::MatchStart(index) => view
                        .buffer()
                        .iter_at_line_index(line, index as i32)
                        .map_or(0.0, |hit| {
                            f64::from(view.iter_location(&hit).x()) - horizontal.page_size() * 0.2
                        }),
                };
                horizontal.set_value(x.max(0.0));
            }
            gtk4::glib::ControlFlow::Break
        });
        self.pending_scroll.replace(Some(callback));
    }

    fn resize_image(&self, reset_scroll: bool) {
        let texture = self.texture.borrow();
        let Some(texture) = texture.as_ref() else {
            return;
        };
        let zoom = ImageZoom::OPTIONS
            .get(self.zoom.selected() as usize)
            .copied()
            .unwrap_or_default();
        let policy = if zoom == ImageZoom::Fit {
            gtk4::PolicyType::Never
        } else {
            gtk4::PolicyType::Automatic
        };
        self.image_scroll.set_policy(policy, policy);
        self.picture.set_can_shrink(zoom == ImageZoom::Fit);
        let scale = zoom.scale().unwrap_or(1.0) / self.picture.scale_factor().max(1) as f32;
        let width = (texture.width() as f32 * scale).round().max(1.0);
        let height = (texture.height() as f32 * scale).round().max(1.0);
        // The snapshot draws the same texture into a paintable whose intrinsic
        // size is the zoomed size. This also lets GtkPicture shrink below 100%.
        let snapshot = gtk4::Snapshot::new();
        snapshot.append_texture(texture, &gtk4::graphene::Rect::new(0.0, 0.0, width, height));
        let paintable = snapshot.to_paintable(Some(&gtk4::graphene::Size::new(width, height)));
        self.picture.set_paintable(paintable.as_ref());
        if reset_scroll {
            self.image_scroll.hadjustment().set_value(0.0);
            self.image_scroll.vadjustment().set_value(0.0);
        }
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
            .map(|line| {
                if line == 1600 {
                    format!("row 1600 {} ROW 1600\n", "x".repeat(220))
                } else {
                    format!("row {line}\n")
                }
            })
            .collect::<String>();
        std::fs::write(&path, &source).unwrap();

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

        panel.find.set_active(true);
        panel.find_query.set_text("row 160");
        assert_eq!(panel.find_count.text(), "2 matches");
        panel.match_case.set_active(false);
        assert_eq!(panel.find_count.text(), "3 matches");
        panel.find_next.emit_clicked();
        assert_eq!(panel.selected_line(), Some(1600));
        panel.find_next.emit_clicked();
        assert_eq!(panel.selected_line(), Some(1600));
        assert_eq!(panel.find_count.text(), "Match 3 of 3");
        wait("horizontal match reveal", &|| {
            panel.text.hadjustment().unwrap().value() > 100.0
        });
        assert!(panel.selected_source_line().unwrap().ends_with("ROW 1600"));
        let hit = panel.matches.borrow().active().unwrap();
        assert!(panel
            .text
            .buffer()
            .iter_at_line_index(1599, hit.start as i32)
            .unwrap()
            .has_tag(&panel.active_match_tag));
        panel.find_next.emit_clicked();
        assert_eq!(panel.selected_line(), Some(160));
        panel.find_previous.emit_clicked();
        assert_eq!(panel.find_count.text(), "Match 3 of 3");
        panel.find.set_active(false);
        panel.jump_to(1400, Some(3), true);
        wait("source navigation after find", &target_visible);

        std::fs::write(&reference, "literal colon filename").unwrap();
        std::fs::remove_file(&path).unwrap();
        panel.reload.emit_clicked();
        assert!(panel.loaded.get() && panel.body.is_visible());
        assert_eq!(panel.text.buffer().line_count(), 1601);
        wait("failed reload", &|| !panel.loading.get());
        assert!(panel
            .read_status
            .text()
            .contains("Showing the last preview"));
        assert_eq!(panel.text.buffer().line_count(), 1601);
        std::fs::write(&path, format!("{source}updated\n")).unwrap();
        panel.reload.emit_clicked();
        wait("source reload", &|| {
            !panel.loading.get() && target_visible()
        });
        assert_eq!(panel.target.borrow().as_ref().unwrap().path, path);
        assert_eq!(panel.text.buffer().line_count(), 1602);

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
        image.zoom.set_selected(1);
        let expected_width = (160.0 / f64::from(image.picture.scale_factor())).round() as i32;
        assert_eq!(
            image.picture.paintable().unwrap().intrinsic_width(),
            expected_width
        );
        image.zoom.set_selected(7);
        wait("image zoom", &|| {
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
