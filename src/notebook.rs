//! Lightweight executable notebook: `.jtnb.md` files = markdown + runnable
//! shell code fences. The viewer is a modal dialog; each ```bash / ```sh /
//! ```shell``` fence becomes a card with a Run button, output is captured
//! inline.
//!
//! Why minimal:
//! - No external markdown crate (offline-build constraint; pulldown_cmark
//!   isn't in the cargo cache). We implement just enough to recognise code
//!   fences and apply trivial styling (headings, bold/italic/inline-code)
//!   via pango markup.
//! - Cells run in an isolated `bash -c` subprocess rooted at the notebook's
//!   own directory — they do NOT touch the user's active terminal. This is
//!   a deliberate trade-off: users get reproducible execution at the cost
//!   of missing their shell aliases. Documented in the dialog footer.
//! - Output captured via two reader threads (stdout/stderr each on their
//!   own thread) feeding an mpsc channel polled on the GLib main loop.
//!   Avoids the classic single-pipe-fills-and-blocks deadlock.

use std::cell::{Cell, RefCell};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use adw::prelude::*;
use relm4::adw;
use relm4::gtk;
use relm4::prelude::*;

/// Max bytes of captured output retained per cell run before truncation.
/// Matches the spirit of `block.rs`'s raw-output cap — bounded memory even
/// for runaway commands.
const MAX_OUTPUT_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Segment {
    /// Plain markdown text (may contain inline formatting we render with
    /// pango markup).
    Text(String),
    /// A fenced code block. `lang` is the optional info string after the
    /// opening fence (e.g. "bash", "rust", "" for an unlabeled fence).
    Code { lang: String, src: String },
}

/// Split a markdown source into text + code segments. We recognise both
/// triple-backtick and triple-tilde fences (CommonMark). Unterminated
/// fences are treated as text — predictable failure mode > erroring out
/// on a partially-edited notebook.
pub(crate) fn parse_segments(input: &str) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    let mut text_buf = String::new();
    let mut lines = input.lines().peekable();

    while let Some(line) = lines.next() {
        if let Some(fence) = fence_marker(line) {
            // Flush pending text.
            if !text_buf.is_empty() {
                out.push(Segment::Text(std::mem::take(&mut text_buf)));
            }
            // Lang is everything after the opening fence.
            let lang = line.trim_start_matches(fence).trim().to_string();
            let mut src = String::new();
            let mut closed = false;
            for inner in lines.by_ref() {
                if fence_marker(inner).map(|m| m == fence).unwrap_or(false) {
                    closed = true;
                    break;
                }
                src.push_str(inner);
                src.push('\n');
            }
            if closed {
                // Strip the trailing \n we added to the last source line
                // for cosmetic consistency in the displayed snippet.
                if src.ends_with('\n') {
                    src.pop();
                }
                out.push(Segment::Code { lang, src });
            } else {
                // Unterminated: put the fence + the rest back as text so
                // the user sees something rather than the cell vanishing.
                text_buf.push_str(line);
                text_buf.push('\n');
                text_buf.push_str(&src);
            }
        } else {
            text_buf.push_str(line);
            text_buf.push('\n');
        }
    }
    if !text_buf.is_empty() {
        out.push(Segment::Text(text_buf));
    }
    out
}

/// Returns the fence marker (``` or ~~~) if the line starts with one
/// (allowing up to 3 leading spaces, per CommonMark). Otherwise None.
fn fence_marker(line: &str) -> Option<&'static str> {
    let stripped = line.trim_start_matches(' ');
    if line.len() - stripped.len() > 3 {
        return None;
    }
    if stripped.starts_with("```") {
        return Some("```");
    }
    if stripped.starts_with("~~~") {
        return Some("~~~");
    }
    None
}

/// Render the markdown body of a `Text` segment to a Pango-markup string.
/// Just enough to look like markdown without pulling in a full crate:
/// `#/##/###` → bolded sized text, `**x**` → bold, `*x*` → italic,
/// `` `x` `` → monospace span. All other text passes through after XML
/// escaping.
pub(crate) fn render_text_to_pango(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for raw_line in text.lines() {
        let line = raw_line.trim_end();
        if line.is_empty() {
            out.push('\n');
            continue;
        }
        // Heading detection — only if the # appears at line start.
        let (open, body, close) = if let Some(rest) = line.strip_prefix("### ") {
            ("<span weight=\"bold\" size=\"large\">", rest, "</span>")
        } else if let Some(rest) = line.strip_prefix("## ") {
            ("<span weight=\"bold\" size=\"x-large\">", rest, "</span>")
        } else if let Some(rest) = line.strip_prefix("# ") {
            ("<span weight=\"bold\" size=\"xx-large\">", rest, "</span>")
        } else {
            ("", line, "")
        };
        out.push_str(open);
        out.push_str(&render_inline(body));
        out.push_str(close);
        out.push('\n');
    }
    out
}

/// Apply inline-format rules. Conservative: only matches the simplest
/// form. Nested or overlapping markers fall through unchanged.
fn render_inline(s: &str) -> String {
    let escaped = escape_pango(s);
    // Backtick spans first so subsequent ** / * passes don't see their
    // interior. We do these as separate scans rather than one big regex
    // to avoid the regex dep and to keep failure modes obvious.
    let with_code = wrap_marker(&escaped, "`", "<tt>", "</tt>");
    let with_bold = wrap_marker(&with_code, "**", "<b>", "</b>");
    wrap_marker(&with_bold, "*", "<i>", "</i>")
}

fn wrap_marker(s: &str, marker: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        let Some(start) = rest.find(marker) else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..start]);
        let after = &rest[start + marker.len()..];
        match after.find(marker) {
            Some(end) => {
                out.push_str(open);
                out.push_str(&after[..end]);
                out.push_str(close);
                rest = &after[end + marker.len()..];
            }
            None => {
                // No closing marker — leave the original alone.
                out.push_str(&rest[start..]);
                return out;
            }
        }
    }
}

fn escape_pango(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Per-cell execution handle so the close-dialog hook can kill in-flight
/// children when the user dismisses the notebook.
struct CellHandle {
    /// Shared with the worker thread so Stop can `kill()` from the UI side.
    /// The child stays in this slot until the worker observes and reaps its
    /// exit; taking it in either caller would make the other side powerless.
    child: Arc<Mutex<Option<Child>>>,
    /// Cross-thread flag — set true when the user clicks Stop / closes the
    /// dialog, read by the worker when deciding whether to report
    /// `cancelled` vs the real exit code.
    cancelled: Arc<AtomicBool>,
}

impl CellHandle {
    /// Request cancellation without taking ownership away from the worker.
    /// `Child::kill` only needs `&mut Child`, so the worker can keep polling the
    /// same shared handle and reap it once the signal has taken effect.
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.child.lock() {
            if let Some(child) = guard.as_mut() {
                let _ = child.kill();
            }
        }
    }
}

#[derive(Debug)]
pub(crate) enum NotebookMsg {
    Open(PathBuf),
    Closed,
}

pub(crate) struct NotebookModel {
    parent: adw::ApplicationWindow,
    handles: Rc<RefCell<Vec<Rc<CellHandle>>>>,
}

#[relm4::component(pub(crate))]
impl Component for NotebookModel {
    type Init = adw::ApplicationWindow;
    type Input = NotebookMsg;
    type Output = ();
    type CommandOutput = ();

    view! {
        root = adw::Dialog {
            set_content_width: 880,
            set_content_height: 680,
            connect_closed => NotebookMsg::Closed,

            #[wrap(Some)]
            set_child = &adw::ToolbarView {
                add_top_bar = &adw::HeaderBar {},

                #[wrap(Some)]
                set_content = &gtk::ScrolledWindow {
                    set_hexpand: true,
                    set_vexpand: true,

                    #[name(content)]
                    gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        set_spacing: 12,
                        set_margin_top: 12,
                        set_margin_bottom: 12,
                        set_margin_start: 16,
                        set_margin_end: 16,
                    },
                },
            },
        }
    }

    fn init(
        parent: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let model = Self {
            parent,
            handles: Rc::new(RefCell::new(Vec::new())),
        };
        let widgets = view_output!();
        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        msg: Self::Input,
        _sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        match msg {
            NotebookMsg::Open(path) => {
                let text = match std::fs::read_to_string(&path) {
                    Ok(text) => text,
                    Err(error) => {
                        log::warn!("notebook: cannot read {}: {error}", path.display());
                        return;
                    }
                };
                self.cancel_cells();
                while let Some(child) = widgets.content.first_child() {
                    widgets.content.remove(&child);
                }
                root.set_title(&format!(
                    "Notebook: {}",
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string())
                ));
                let cwd = path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                for segment in parse_segments(&text) {
                    match segment {
                        Segment::Text(text) => {
                            let label = gtk::Label::new(None);
                            label.set_use_markup(true);
                            label.set_markup(&render_text_to_pango(&text));
                            label.set_wrap(true);
                            label.set_xalign(0.0);
                            label.set_halign(gtk::Align::Fill);
                            label.set_selectable(true);
                            widgets.content.append(&label);
                        }
                        Segment::Code { lang, src } => widgets.content.append(&build_code_cell(
                            &lang,
                            &src,
                            &cwd,
                            &self.handles,
                        )),
                    }
                }
                let footer = gtk::Label::new(Some(
                    "Cells run in an isolated `bash -c` rooted at the notebook's directory. \
                     Shell init scripts (.bashrc, aliases, rsh) are not loaded.",
                ));
                footer.set_wrap(true);
                footer.set_xalign(0.0);
                footer.add_css_class("dim-label");
                widgets.content.append(&footer);
                root.present(Some(&self.parent));
            }
            NotebookMsg::Closed => self.cancel_cells(),
        }
    }
}

impl NotebookModel {
    fn cancel_cells(&self) {
        for handle in self.handles.borrow_mut().drain(..) {
            handle.cancel();
        }
    }
}

fn build_code_cell(
    lang: &str,
    src: &str,
    cwd: &Path,
    handles: &Rc<RefCell<Vec<Rc<CellHandle>>>>,
) -> gtk::Frame {
    let runnable = matches!(
        lang.to_ascii_lowercase().as_str(),
        "bash" | "sh" | "shell" | ""
    );

    let frame = gtk::Frame::new(None);
    frame.add_css_class("card");
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 4);
    vbox.set_margin_top(8);
    vbox.set_margin_bottom(8);
    vbox.set_margin_start(8);
    vbox.set_margin_end(8);

    // Top row: language label + toolbar buttons.
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let lang_label = gtk::Label::new(Some(if lang.is_empty() { "shell" } else { lang }));
    lang_label.add_css_class("dim-label");
    lang_label.set_xalign(0.0);
    lang_label.set_hexpand(true);
    top.append(&lang_label);

    let copy_btn = gtk::Button::with_label("Copy");
    copy_btn.add_css_class("flat");
    {
        let src = src.to_string();
        copy_btn.connect_clicked(move |_| {
            if let Some(display) = gtk::gdk::Display::default() {
                display.clipboard().set_text(&src);
            }
        });
    }
    let run_btn = gtk::Button::with_label("Run");
    if runnable {
        run_btn.add_css_class("suggested-action");
    } else {
        run_btn.set_sensitive(false);
        run_btn.set_tooltip_text(Some("Only bash / sh / shell fences are runnable"));
    }
    let stop_btn = gtk::Button::with_label("Stop");
    stop_btn.set_sensitive(false);
    top.append(&copy_btn);
    top.append(&run_btn);
    top.append(&stop_btn);
    vbox.append(&top);

    // Source view: monospace, read-only.
    let src_buffer = gtk::TextBuffer::new(None);
    src_buffer.set_text(src);
    let src_view = gtk::TextView::with_buffer(&src_buffer);
    src_view.set_editable(false);
    src_view.set_monospace(true);
    src_view.set_cursor_visible(false);
    src_view.set_wrap_mode(gtk::WrapMode::None);
    src_view.add_css_class("notebook-source");
    let src_scroll = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .max_content_height(220)
        .child(&src_view)
        .build();
    src_scroll.set_propagate_natural_height(true);
    vbox.append(&src_scroll);

    // Output area, initially hidden until first Run.
    let output_buffer = gtk::TextBuffer::new(None);
    let output_view = gtk::TextView::with_buffer(&output_buffer);
    output_view.set_editable(false);
    output_view.set_monospace(true);
    output_view.set_cursor_visible(false);
    output_view.set_wrap_mode(gtk::WrapMode::WordChar);
    output_view.add_css_class("notebook-output");
    let output_scroll = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .max_content_height(300)
        .child(&output_view)
        .build();
    output_scroll.set_propagate_natural_height(true);
    output_scroll.set_visible(false);
    vbox.append(&output_scroll);

    let status_label = gtk::Label::new(None);
    status_label.set_xalign(0.0);
    status_label.add_css_class("dim-label");
    status_label.set_visible(false);
    vbox.append(&status_label);

    frame.set_child(Some(&vbox));

    // Each Stop button must target its own cell. Looking through the dialog's
    // global handle list can stop a different cell when several run at once.
    // Installing this before the worker starts also closes the tiny race where
    // Stop is clicked before `Command::spawn` has parked the Child handle.
    let active_handle: Rc<RefCell<Option<Rc<CellHandle>>>> = Rc::new(RefCell::new(None));

    if runnable {
        let src = src.to_string();
        let cwd = cwd.to_path_buf();
        let handles = handles.clone();
        let run_btn_c = run_btn.clone();
        let stop_btn_c = stop_btn.clone();
        let output_buffer_c = output_buffer.clone();
        let output_scroll_c = output_scroll.clone();
        let status_label_c = status_label.clone();
        let active_handle_c = active_handle.clone();
        run_btn.connect_clicked(move |_| {
            // Reset previous output.
            output_buffer_c.set_text("");
            output_scroll_c.set_visible(true);
            status_label_c.set_visible(true);
            status_label_c.set_text("Running…");
            status_label_c.remove_css_class("error");
            run_btn_c.set_sensitive(false);
            stop_btn_c.set_sensitive(true);

            let handle = Rc::new(CellHandle {
                child: Arc::new(Mutex::new(None)),
                cancelled: Arc::new(AtomicBool::new(false)),
            });
            *active_handle_c.borrow_mut() = Some(handle.clone());
            handles.borrow_mut().push(handle.clone());

            spawn_cell(
                src.clone(),
                cwd.clone(),
                handle.clone(),
                output_buffer_c.clone(),
                output_scroll_c.clone(),
                status_label_c.clone(),
                run_btn_c.clone(),
                stop_btn_c.clone(),
            );
        });
    }

    {
        let active_handle_for_stop = active_handle;
        stop_btn.connect_clicked(move |btn| {
            if let Some(handle) = active_handle_for_stop.borrow().as_ref() {
                handle.cancel();
            }
            btn.set_sensitive(false);
        });
    }

    frame
}

#[allow(clippy::too_many_arguments)]
fn spawn_cell(
    src: String,
    cwd: std::path::PathBuf,
    handle: Rc<CellHandle>,
    output_buffer: gtk::TextBuffer,
    output_scroll: gtk::ScrolledWindow,
    status_label: gtk::Label,
    run_btn: gtk::Button,
    stop_btn: gtk::Button,
) {
    // Two channels: chunks of bytes for incremental output, and the final
    // exit result. We could collapse to one, but the exit signal sits
    // cleaner as a separate enum case.
    let (chunk_tx, chunk_rx) = mpsc::channel::<Vec<u8>>();
    let (done_tx, done_rx) = mpsc::channel::<Result<i32, String>>();

    let child_slot = handle.child.clone();
    let cancelled_flag = handle.cancelled.clone();
    std::thread::spawn(move || {
        let mut command = Command::new("bash");
        command
            .arg("-c")
            .arg(&src)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = done_tx.send(Err(format!("spawn failed: {e}")));
                return;
            }
        };

        // Park the Child so the UI thread can kill() it. Cancellation can win
        // the race before spawn completes; honour the flag while holding the
        // first lock so such a child cannot escape into the background.
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        match child_slot.lock() {
            Ok(mut guard) => {
                *guard = Some(child);
                if cancelled_flag.load(Ordering::SeqCst) {
                    if let Some(child) = guard.as_mut() {
                        let _ = child.kill();
                    }
                }
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = done_tx.send(Err("child handle mutex poisoned".to_string()));
                return;
            }
        }

        // stdout reader.
        let chunk_tx_o = chunk_tx.clone();
        let stdout_thread = stdout.map(|mut so| {
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match so.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if chunk_tx_o.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
        });

        // stderr reader (merged into the same output buffer — UI doesn't
        // distinguish streams in v1; the exit colour conveys success/fail).
        let chunk_tx_e = chunk_tx.clone();
        let stderr_thread = stderr.map(|mut se| {
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match se.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if chunk_tx_e.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
        });

        // Poll under short locks rather than taking the Child and blocking in
        // `wait()`. This leaves Stop/close able to call `kill()` throughout the
        // run, while this worker remains the sole reaper.
        let exit = wait_for_shared_child(&child_slot);

        // Drain readers before we report done so chunks don't arrive after
        // the exit notification (UI order-sensitive).
        if let Some(jh) = stdout_thread {
            let _ = jh.join();
        }
        if let Some(jh) = stderr_thread {
            let _ = jh.join();
        }
        drop(chunk_tx);

        let result = match exit {
            Ok(code) => {
                if cancelled_flag.load(Ordering::SeqCst) {
                    Err("cancelled".to_string())
                } else {
                    Ok(code)
                }
            }
            Err(e) => Err(format!("wait failed: {e}")),
        };
        let _ = done_tx.send(result);
    });

    // Poll the channels on the main loop. 50ms is responsive without
    // hammering the UI.
    let bytes_seen: Rc<Cell<usize>> = Rc::new(Cell::new(0));
    let truncated: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let output_buffer_t = output_buffer.clone();
    let output_scroll_t = output_scroll.clone();
    let status_label_t = status_label.clone();
    let run_btn_t = run_btn.clone();
    let stop_btn_t = stop_btn.clone();
    gtk::glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
        // Drain available chunks.
        loop {
            match chunk_rx.try_recv() {
                Ok(bytes) => {
                    let already = bytes_seen.get();
                    if already >= MAX_OUTPUT_BYTES {
                        if !truncated.get() {
                            truncated.set(true);
                            let mut end = output_buffer_t.end_iter();
                            output_buffer_t.insert(&mut end, "\n[output truncated]\n");
                        }
                        continue;
                    }
                    let remaining = MAX_OUTPUT_BYTES.saturating_sub(already);
                    let take = bytes.len().min(remaining);
                    bytes_seen.set(already + take);
                    let slice = &bytes[..take];
                    let text = String::from_utf8_lossy(slice);
                    let mut end = output_buffer_t.end_iter();
                    output_buffer_t.insert(&mut end, &text);
                    // Autoscroll.
                    let vadj = output_scroll_t.vadjustment();
                    vadj.set_value(vadj.upper());
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }

        match done_rx.try_recv() {
            Ok(Ok(code)) => {
                if code == 0 {
                    status_label_t.set_text(&format!("exit {code}"));
                    status_label_t.remove_css_class("error");
                } else {
                    status_label_t.set_text(&format!("exit {code}"));
                    status_label_t.add_css_class("error");
                }
                run_btn_t.set_sensitive(true);
                stop_btn_t.set_sensitive(false);
                gtk::glib::ControlFlow::Break
            }
            Ok(Err(reason)) => {
                status_label_t.set_text(&format!("failed: {reason}"));
                status_label_t.add_css_class("error");
                run_btn_t.set_sensitive(true);
                stop_btn_t.set_sensitive(false);
                gtk::glib::ControlFlow::Break
            }
            Err(mpsc::TryRecvError::Empty) => gtk::glib::ControlFlow::Continue,
            Err(mpsc::TryRecvError::Disconnected) => gtk::glib::ControlFlow::Break,
        }
    });
}

/// Wait for a shared child without holding the mutex between polls. The UI can
/// therefore acquire the same handle and call `kill()` at any point. Once an
/// exit is observed, `try_wait` has reaped it and we clear the slot.
fn wait_for_shared_child(child_slot: &Arc<Mutex<Option<Child>>>) -> std::io::Result<i32> {
    loop {
        let result = {
            let mut guard = child_slot
                .lock()
                .map_err(|_| std::io::Error::other("child handle mutex poisoned"))?;
            let child = guard
                .as_mut()
                .ok_or_else(|| std::io::Error::other("child handle missing before exit"))?;
            match child.try_wait()? {
                Some(status) => {
                    let code = status.code().unwrap_or(-1);
                    guard.take();
                    Some(code)
                }
                None => None,
            }
        };
        if let Some(code) = result {
            return Ok(code);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_text_and_code_fences() {
        let md = "Intro line\n```bash\necho hi\n```\nMiddle\n```\nls\n```\ntail";
        let segs = parse_segments(md);
        assert_eq!(segs.len(), 5);
        assert!(matches!(segs[0], Segment::Text(_)));
        assert!(matches!(segs[1], Segment::Code { .. }));
        assert!(matches!(segs[2], Segment::Text(_)));
        assert!(matches!(segs[3], Segment::Code { .. }));
        assert!(matches!(segs[4], Segment::Text(_)));
        if let Segment::Code { lang, src } = &segs[1] {
            assert_eq!(lang, "bash");
            assert_eq!(src, "echo hi");
        }
        if let Segment::Code { lang, src } = &segs[3] {
            assert_eq!(lang, "");
            assert_eq!(src, "ls");
        }
    }

    #[test]
    fn tilde_fences_recognised() {
        let md = "~~~sh\nwhoami\n~~~\n";
        let segs = parse_segments(md);
        assert_eq!(segs.len(), 1);
        assert!(matches!(segs[0], Segment::Code { .. }));
    }

    #[test]
    fn unterminated_fence_falls_back_to_text() {
        let md = "before\n```bash\necho oops\nno closing fence here\n";
        let segs = parse_segments(md);
        // Should be one big text segment, not a code segment.
        assert!(segs.iter().all(|s| matches!(s, Segment::Text(_))));
    }

    #[test]
    fn empty_input_yields_no_segments() {
        assert!(parse_segments("").is_empty());
    }

    #[test]
    fn escape_pango_handles_ampersand_and_angles() {
        assert_eq!(escape_pango("a & b < c > d"), "a &amp; b &lt; c &gt; d");
    }

    #[test]
    fn render_inline_bolds_and_italicises() {
        let r = render_inline("look at **this** and *that*");
        assert!(r.contains("<b>this</b>"), "got {r}");
        assert!(r.contains("<i>that</i>"), "got {r}");
    }

    #[test]
    fn render_inline_wraps_backtick_code() {
        let r = render_inline("run `ls -la` please");
        assert!(r.contains("<tt>ls -la</tt>"), "got {r}");
    }

    #[test]
    fn render_inline_leaves_unmatched_markers_alone() {
        // A single unmatched ** must not produce dangling tags or panic.
        let r = render_inline("oops **forgot to close");
        assert!(!r.contains("<b>"), "got {r}");
    }

    #[test]
    fn render_text_to_pango_handles_headings() {
        let out = render_text_to_pango("# Title\n\nbody");
        assert!(out.contains("Title</span>"), "got {out}");
    }

    #[test]
    fn fence_with_leading_spaces_recognised() {
        let md = "  ```bash\necho ok\n  ```\n";
        let segs = parse_segments(md);
        assert!(
            matches!(segs.first(), Some(Segment::Code { .. })),
            "got {segs:?}"
        );
    }

    #[test]
    fn cancel_leaves_child_for_worker_to_reap() {
        let child = Command::new("bash")
            .args(["-c", "exec sleep 30"])
            .spawn()
            .expect("spawn cancellable child");
        let handle = CellHandle {
            child: Arc::new(Mutex::new(Some(child))),
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        handle.cancel();

        assert!(handle.cancelled.load(Ordering::SeqCst));
        assert!(
            handle.child.lock().expect("child slot").is_some(),
            "cancel must not take the Child away from the waiting worker"
        );

        let started = std::time::Instant::now();
        let code = wait_for_shared_child(&handle.child).expect("reap killed child");
        assert_eq!(code, -1, "a signal-terminated child has no exit code");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "cancelled child was not reaped promptly"
        );
        assert!(
            handle.child.lock().expect("child slot").is_none(),
            "worker must clear the slot after reaping"
        );
    }
}
