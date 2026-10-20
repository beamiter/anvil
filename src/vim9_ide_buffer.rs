//! Buffer text is shaped by Pango and painted by GTK from Vim's semantic RGB
//! render frames. No terminal cells, ANSI escapes, or VTE palette entries are
//! consulted here. VTE remains the PTY/input adapter and compatibility view.

use super::gtk;
use gtk::{gdk, glib, pango, subclass::prelude::*};
use jterm_core::vim9::{Vim9Command, Vim9RenderFrame, Vim9RenderStyle, Vim9Session};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use vte4::prelude::*;

const DEFAULT_FOREGROUND: u32 = 0xd9e0ee;
const DEFAULT_BACKGROUND: u32 = 0x121720;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Metrics {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

#[derive(Clone)]
struct Preedit {
    text: String,
    attributes: pango::AttrList,
    cursor: i32,
}

struct PaintState {
    frame: Option<Arc<Vim9RenderFrame>>,
    metrics: Metrics,
    font: pango::FontDescription,
    ligatures: bool,
    focused: bool,
    preedit: Option<Preedit>,
}

impl Default for PaintState {
    fn default() -> Self {
        Self {
            frame: None,
            metrics: Metrics::default(),
            font: pango::FontDescription::from_string("Monospace 12"),
            ligatures: true,
            focused: false,
            preedit: None,
        }
    }
}

type LayoutKey = (String, bool, bool);

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct BufferSurface {
        pub(super) state: RefCell<PaintState>,
        pub(super) layouts: RefCell<HashMap<LayoutKey, pango::Layout>>,
        #[cfg(test)]
        pub(super) painted_revision: Cell<u64>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for BufferSurface {
        const NAME: &'static str = "AnvilVim9BufferSurface";
        type Type = super::BufferSurface;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for BufferSurface {}

    impl WidgetImpl for BufferSurface {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let surface = self.obj();
            let state = self.state.borrow();
            let Some(frame) = state.frame.as_ref() else {
                return;
            };
            let metrics = state.metrics;
            if metrics.width <= 0.0 || metrics.height <= 0.0 {
                return;
            }
            let view = gtk::graphene::Rect::new(
                metrics.x + frame.view.column as f32 * metrics.width,
                metrics.y + frame.view.row as f32 * metrics.height,
                frame.view.columns as f32 * metrics.width,
                frame.view.rows as f32 * metrics.height,
            );
            snapshot.push_clip(&view);
            let (_, background, _) = colors(&frame.default_style, &frame.default_style);
            snapshot.append_color(&rgb(background), &view);

            // Paint backgrounds before ink so a subsequent syntax run cannot
            // erase italic glyphs extending beyond the previous run's box.
            for run in &frame.runs {
                let (_, background, _) = colors(&run.style, &frame.default_style);
                snapshot.append_color(
                    &rgb(background),
                    &gtk::graphene::Rect::new(
                        metrics.x + run.column as f32 * metrics.width,
                        metrics.y + run.row as f32 * metrics.height,
                        run.cells as f32 * metrics.width,
                        metrics.height,
                    ),
                );
            }

            // Layouts are cached across cursor and viewport updates. Pango
            // shapes a complete same-style run, including font fallback and
            // supported ligatures; GTK retains device-independent text nodes.
            let mut layouts = self.layouts.borrow_mut();
            if layouts.len() > 4096 {
                layouts.clear();
            }
            for run in &frame.runs {
                let x = metrics.x + run.column as f32 * metrics.width;
                let y = metrics.y + run.row as f32 * metrics.height;
                let width = run.cells as f32 * metrics.width;
                let (foreground, _, special) = colors(&run.style, &frame.default_style);
                if !run.text.is_empty() && width > 0.0 {
                    let key = (run.text.clone(), run.style.bold, run.style.italic);
                    let layout = layouts.entry(key).or_insert_with(|| {
                        let layout = surface.create_pango_layout(Some(&run.text));
                        let mut font = state.font.clone();
                        if run.style.bold {
                            font.set_weight(pango::Weight::Bold);
                        }
                        if run.style.italic {
                            font.set_style(pango::Style::Italic);
                        }
                        layout.set_font_description(Some(&font));
                        layout.set_single_paragraph_mode(true);
                        layout.set_auto_dir(false);
                        let attributes = pango::AttrList::new();
                        attributes.insert(pango::AttrFontFeatures::new(if state.ligatures {
                            "calt=1,liga=1"
                        } else {
                            "calt=0,liga=0"
                        }));
                        layout.set_attributes(Some(&attributes));
                        layout
                    });
                    let (_, logical) = layout.extents();
                    let logical_width = logical.width() as f32 / pango::SCALE as f32;
                    let logical_height = logical.height() as f32 / pango::SCALE as f32;
                    let text_y = y + (metrics.height - logical_height) * 0.5;
                    snapshot.save();
                    snapshot.translate(&gtk::graphene::Point::new(x, text_y));
                    // VTE rounds its input grid to whole logical pixels. Match
                    // that advance without independently slicing Unicode or
                    // breaking a shaped run into terminal-sized glyph boxes.
                    if logical_width > 0.0 {
                        snapshot.scale(width / logical_width, 1.0);
                    }
                    snapshot.append_layout(layout, &rgb(foreground));
                    snapshot.restore();
                    decorations(snapshot, &run.style, special, x, y, width, metrics.height);
                }
            }

            if frame.cursor.visible {
                let cursor = gtk::graphene::Rect::new(
                    metrics.x + frame.cursor.column as f32 * metrics.width,
                    metrics.y + frame.cursor.row as f32 * metrics.height,
                    frame.cursor.width.max(1) as f32 * metrics.width,
                    metrics.height,
                );
                let (foreground, _, _) = colors(&frame.default_style, &frame.default_style);
                paint_cursor(snapshot, &cursor, foreground, &frame.mode, state.focused);
                if let Some(preedit) = state.preedit.as_ref() {
                    paint_preedit(&surface, snapshot, &state, preedit, &cursor, &view);
                }
            }
            snapshot.pop();
            #[cfg(test)]
            self.painted_revision.set(frame.revision);
        }
    }
}

glib::wrapper! {
    pub struct BufferSurface(ObjectSubclass<imp::BufferSurface>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

fn rgb(value: u32) -> gdk::RGBA {
    gdk::RGBA::new(
        ((value >> 16) & 255) as f32 / 255.0,
        ((value >> 8) & 255) as f32 / 255.0,
        (value & 255) as f32 / 255.0,
        1.0,
    )
}

fn colors(style: &Vim9RenderStyle, normal: &Vim9RenderStyle) -> (u32, u32, u32) {
    let foreground = style
        .foreground
        .or(normal.foreground)
        .unwrap_or(DEFAULT_FOREGROUND);
    let background = style
        .background
        .or(normal.background)
        .unwrap_or(DEFAULT_BACKGROUND);
    let (foreground, background) = if style.reverse {
        (background, foreground)
    } else {
        (foreground, background)
    };
    (
        foreground,
        background,
        style.special.or(normal.special).unwrap_or(foreground),
    )
}

fn decorations(
    snapshot: &gtk::Snapshot,
    style: &Vim9RenderStyle,
    color: u32,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
) {
    let color = rgb(color);
    if style.underline {
        snapshot.append_color(
            &color,
            &gtk::graphene::Rect::new(x, y + height - 2.0, width, 1.0),
        );
    }
    if style.strikethrough {
        snapshot.append_color(
            &color,
            &gtk::graphene::Rect::new(x, y + height * 0.55, width, 1.0),
        );
    }
    if style.undercurl {
        let rect = gtk::graphene::Rect::new(x, y, width, height);
        let cr = snapshot.append_cairo(&rect);
        cr.set_source_rgb(
            color.red() as f64,
            color.green() as f64,
            color.blue() as f64,
        );
        cr.set_line_width(1.0);
        cr.move_to(x as f64, (y + height - 2.0) as f64);
        let mut offset = 0.0;
        while offset < width {
            let middle = (offset + 2.0).min(width);
            let end = (offset + 4.0).min(width);
            cr.line_to((x + middle) as f64, (y + height - 4.0) as f64);
            cr.line_to((x + end) as f64, (y + height - 2.0) as f64);
            offset += 4.0;
        }
        let _ = cr.stroke();
    }
}

fn paint_cursor(
    snapshot: &gtk::Snapshot,
    rect: &gtk::graphene::Rect,
    color: u32,
    mode: &str,
    focused: bool,
) {
    let mut color = rgb(color);
    if !focused {
        for edge in [
            gtk::graphene::Rect::new(rect.x(), rect.y(), rect.width(), 1.0),
            gtk::graphene::Rect::new(rect.x(), rect.y() + rect.height() - 1.0, rect.width(), 1.0),
            gtk::graphene::Rect::new(rect.x(), rect.y(), 1.0, rect.height()),
            gtk::graphene::Rect::new(rect.x() + rect.width() - 1.0, rect.y(), 1.0, rect.height()),
        ] {
            snapshot.append_color(&color, &edge);
        }
    } else if mode.starts_with('i') {
        snapshot.append_color(
            &color,
            &gtk::graphene::Rect::new(rect.x(), rect.y(), 2.0, rect.height()),
        );
    } else if mode.starts_with('R') {
        snapshot.append_color(
            &color,
            &gtk::graphene::Rect::new(rect.x(), rect.y() + rect.height() - 3.0, rect.width(), 3.0),
        );
    } else {
        color.set_alpha(0.38);
        snapshot.append_color(&color, rect);
    }
}

fn paint_preedit(
    surface: &BufferSurface,
    snapshot: &gtk::Snapshot,
    state: &PaintState,
    preedit: &Preedit,
    cursor: &gtk::graphene::Rect,
    view: &gtk::graphene::Rect,
) {
    let Some(frame) = state.frame.as_ref() else {
        return;
    };
    let layout = surface.create_pango_layout(Some(&preedit.text));
    layout.set_font_description(Some(&state.font));
    layout.set_attributes(Some(&preedit.attributes));
    layout.set_single_paragraph_mode(true);
    let (width, height) = layout.pixel_size();
    let x = cursor
        .x()
        .min((view.x() + view.width() - width as f32 - 4.0).max(view.x()));
    let y = cursor.y() + (state.metrics.height - height as f32) * 0.5;
    let (foreground, background, _) = colors(&frame.default_style, &frame.default_style);
    snapshot.append_color(
        &rgb(background),
        &gtk::graphene::Rect::new(x, cursor.y(), width as f32 + 4.0, state.metrics.height),
    );
    snapshot.save();
    snapshot.translate(&gtk::graphene::Point::new(x, y));
    snapshot.append_layout(&layout, &rgb(foreground));
    snapshot.restore();
    snapshot.append_color(
        &rgb(foreground),
        &gtk::graphene::Rect::new(
            x,
            cursor.y() + state.metrics.height - 1.0,
            width as f32,
            1.0,
        ),
    );
    let byte = preedit
        .text
        .char_indices()
        .nth(preedit.cursor.max(0) as usize)
        .map_or(preedit.text.len(), |(byte, _)| byte);
    let position = layout.index_to_pos(byte.min(i32::MAX as usize) as i32);
    let caret_x = x + position.x() as f32 / pango::SCALE as f32;
    snapshot.append_color(
        &rgb(foreground),
        &gtk::graphene::Rect::new(caret_x, cursor.y(), 1.0, state.metrics.height),
    );
}

impl BufferSurface {
    fn new() -> Self {
        let surface: Self = glib::Object::builder().build();
        surface.set_widget_name("vim9-native-buffer");
        surface.set_hexpand(true);
        surface.set_vexpand(true);
        surface.set_can_target(false);
        surface.set_focusable(false);
        surface.update_property(&[gtk::accessible::Property::Label(
            "Native Vim buffer rendering",
        )]);
        surface
    }

    fn set_font(&self, font: &pango::FontDescription) {
        self.imp().state.borrow_mut().font = font.clone();
        self.imp().layouts.borrow_mut().clear();
        self.queue_draw();
    }

    fn set_preedit(&self, preedit: Option<Preedit>) {
        self.imp().state.borrow_mut().preedit = preedit;
        self.queue_draw();
    }

    pub(super) fn is_preediting(&self) -> bool {
        self.imp().state.borrow().preedit.is_some()
    }

    #[cfg(test)]
    pub(super) fn painted_frame(&self) -> Option<Arc<Vim9RenderFrame>> {
        let frame = self.imp().state.borrow().frame.clone()?;
        (self.imp().painted_revision.get() == frame.revision).then_some(frame)
    }
}

pub(super) struct NativeBuffer {
    pub widget: gtk::Overlay,
    pub menu: gtk::MenuButton,
    pub badge: gtk::Label,
    pub surface: BufferSurface,
}

impl NativeBuffer {
    pub(super) fn new(session: &Vim9Session, terminal: &vte4::Terminal) -> Self {
        let widget = gtk::Overlay::new();
        widget.set_child(Some(terminal));
        let surface = BufferSurface::new();
        widget.add_overlay(&surface);
        widget.set_measure_overlay(&surface, false);
        let badge = gtk::Label::new(Some("GUI · connecting"));
        badge.add_css_class("caption");
        badge.set_ellipsize(pango::EllipsizeMode::End);
        badge.set_max_width_chars(28);
        badge.set_single_line_mode(true);
        badge.set_tooltip_text(Some(
            "The editor buffer is drawn from Vim GUI highlight colors.",
        ));

        let menu = gtk::MenuButton::new();
        menu.set_icon_name("preferences-desktop-font-symbolic");
        menu.set_tooltip_text(Some("Editor rendering, font and line spacing"));
        let popover = gtk::Popover::new();
        let settings = gtk::Box::new(gtk::Orientation::Vertical, 10);
        settings.set_margin_start(14);
        settings.set_margin_end(14);
        settings.set_margin_top(14);
        settings.set_margin_bottom(14);
        let enabled = gtk::CheckButton::with_label("Native GUI buffer");
        enabled.set_active(true);
        enabled.set_tooltip_text(Some("Use Vim's GUI RGB highlights and Pango text rendering. Unsupported views use Vim's compatibility display."));
        settings.append(&enabled);
        let font_dialog = gtk::FontDialog::new();
        font_dialog.set_title("Editor font");
        font_dialog.set_filter(Some(&gtk::CustomFilter::new(|object| {
            object.downcast_ref::<pango::FontFamily>().map_or_else(
                || {
                    object
                        .downcast_ref::<pango::FontFace>()
                        .is_some_and(|face| face.family().is_monospace())
                },
                |family| family.is_monospace(),
            )
        })));
        let font = gtk::FontDialogButton::new(Some(font_dialog));
        font.set_font_desc(&pango::FontDescription::from_string("Monospace 12"));
        font.set_tooltip_text(Some(
            "Editor font; workspace and sidebar fonts keep their own settings",
        ));
        settings.append(&font);
        let ligatures = gtk::CheckButton::with_label("Font ligatures");
        ligatures.set_active(true);
        settings.append(&ligatures);
        let spacing_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        spacing_row.append(&gtk::Label::new(Some("Line spacing")));
        let spacing = gtk::SpinButton::with_range(1.0, 1.8, 0.05);
        spacing.set_digits(2);
        spacing.set_value(1.15);
        spacing.set_tooltip_text(Some("Line height multiplier for this editor"));
        terminal.set_cell_height_scale(1.15);
        spacing_row.append(&spacing);
        settings.append(&spacing_row);
        popover.set_child(Some(&settings));
        menu.set_popover(Some(&popover));

        // A single native IM context owns composition while the native buffer
        // is visible. Commits still enter the real Vim PTY; ordinary special
        // keys fall through to VTE. Otherwise VTE would draw preedit beneath
        // the opaque native buffer and hide Chinese/Japanese composition.
        let im = gtk::IMMulticontext::new();
        im.set_client_widget(Some(terminal));
        im.set_use_preedit(true);
        let im_keys = gtk::EventControllerKey::new();
        im_keys.set_name(Some("vim9-native-input-method"));
        im_keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        terminal.add_controller(im_keys.clone());
        {
            let weak_terminal = terminal.downgrade();
            let weak_surface = surface.downgrade();
            im.connect_commit(move |_, text| {
                if let Some(terminal) = weak_terminal.upgrade() {
                    terminal.feed_child(text.as_bytes());
                }
                if let Some(surface) = weak_surface.upgrade() {
                    surface.set_preedit(None);
                }
            });
        }
        {
            let weak_surface = surface.downgrade();
            im.connect_preedit_changed(move |im| {
                if let Some(surface) = weak_surface.upgrade() {
                    let (text, attributes, cursor) = im.preedit_string();
                    surface.set_preedit((!text.is_empty()).then(|| Preedit {
                        text: text.into(),
                        attributes,
                        cursor,
                    }));
                }
            });
        }
        {
            let weak_surface = surface.downgrade();
            im.connect_preedit_end(move |_| {
                if let Some(surface) = weak_surface.upgrade() {
                    surface.set_preedit(None);
                }
            });
        }
        {
            let terminal = terminal.clone();
            let surface = surface.clone();
            font.connect_font_desc_notify(move |font| {
                if let Some(font) = font.font_desc() {
                    terminal.set_font(Some(&font));
                    surface.set_font(&font);
                }
            });
        }
        {
            let terminal = terminal.clone();
            spacing.connect_value_changed(move |spacing| {
                terminal.set_cell_height_scale(spacing.value())
            });
        }
        {
            let surface = surface.clone();
            ligatures.connect_toggled(move |button| {
                surface.imp().state.borrow_mut().ligatures = button.is_active();
                surface.imp().layouts.borrow_mut().clear();
                surface.queue_draw();
            });
        }

        let im_active = Rc::new(Cell::new(false));
        {
            let im = im.clone();
            let im_active = im_active.clone();
            let surface = surface.clone();
            terminal.connect_has_focus_notify(move |terminal| {
                if im_active.get() {
                    if terminal.has_focus() {
                        im.focus_in();
                    } else {
                        im.focus_out();
                    }
                }
                surface.imp().state.borrow_mut().focused = terminal.has_focus();
                surface.queue_draw();
            });
        }
        let session = session.clone();
        let weak_widget = widget.downgrade();
        let weak_terminal = terminal.downgrade();
        let poll_surface = surface.clone();
        let poll_badge = badge.clone();
        let mut subscribed = None;
        let mut old_status = String::new();
        glib::timeout_add_local(Duration::from_millis(16), move || {
            if weak_widget.upgrade().is_none() {
                return glib::ControlFlow::Break;
            }
            let Some(terminal) = weak_terminal.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let wanted = enabled.is_active();
            // send() is bounded and refuses unauthenticated sessions; retry
            // subscription after startup rather than silently losing it.
            if subscribed != Some(wanted)
                && session
                    .send(Vim9Command::SetRender { enabled: wanted })
                    .is_ok()
            {
                subscribed = Some(wanted);
            }
            let frame = session.render_snapshot();
            let geometry_matches = frame.as_ref().is_some_and(|frame| {
                frame.rows == terminal.row_count().max(0) as usize
                    && frame.columns == terminal.column_count().max(0) as usize
            });
            let terminal_selection = terminal.has_selection();
            let active = wanted
                && !terminal_selection
                && geometry_matches
                && frame.as_ref().is_some_and(|frame| frame.supported);
            let metrics = terminal_metrics(&terminal);
            let next = if active { frame.clone() } else { None };
            let mut state = poll_surface.imp().state.borrow_mut();
            let changed = match (&state.frame, &next) {
                (Some(previous), Some(next)) => !Arc::ptr_eq(previous, next),
                (None, None) => false,
                _ => true,
            } || state.metrics != metrics;
            state.metrics = metrics;
            state.frame = next;
            state.focused = terminal.has_focus();
            if !active {
                state.preedit = None;
            }
            drop(state);
            if changed {
                poll_surface.queue_draw();
            }

            if im_active.replace(active) != active {
                if active {
                    im_keys.set_im_context(Some(&im));
                    if terminal.has_focus() {
                        im.focus_in();
                    }
                } else {
                    im.focus_out();
                    im.reset();
                    im_keys.set_im_context(None::<&gtk::IMContext>);
                }
            }
            if active {
                if let Some(frame) = frame.as_ref() {
                    im.set_cursor_location(&gdk::Rectangle::new(
                        (metrics.x + frame.cursor.column as f32 * metrics.width).round() as i32,
                        (metrics.y + frame.cursor.row as f32 * metrics.height).round() as i32,
                        metrics.width.ceil() as i32,
                        metrics.height.ceil() as i32,
                    ));
                }
            }
            let status = if !wanted {
                "Vim · compatibility display".into()
            } else if terminal_selection {
                "Vim · terminal text selection".into()
            } else if active {
                "GUI · RGB / Pango".into()
            } else if let Some(frame) = frame.as_ref() {
                if !frame.supported {
                    format!("Vim · {}", super::visible(&frame.fallback_reason))
                } else {
                    "GUI · resizing".into()
                }
            } else {
                "GUI · connecting".into()
            };
            if status != old_status {
                poll_badge.set_label(&status);
                poll_badge.set_tooltip_text(Some(&status));
                old_status = status;
            }
            glib::ControlFlow::Continue
        });
        Self {
            widget,
            menu,
            badge,
            surface,
        }
    }
}

fn terminal_metrics(terminal: &vte4::Terminal) -> Metrics {
    let border = gtk::prelude::ScrollableExt::border(terminal);
    let (x, y) = border.as_ref().map_or((0.0, 0.0), |border| {
        (border.left() as f32, border.top() as f32)
    });
    Metrics {
        x,
        y,
        width: terminal.char_width().max(1) as f32,
        height: terminal.char_height().max(1) as f32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_keep_full_gui_rgb_and_inherit_before_reverse() {
        let normal = Vim9RenderStyle {
            foreground: Some(0x123456),
            background: Some(0x789abc),
            ..Default::default()
        };
        let syntax = Vim9RenderStyle {
            foreground: Some(0xd18cf7),
            special: Some(0x26a5d3),
            reverse: true,
            ..Default::default()
        };
        assert_eq!(colors(&syntax, &normal), (0x789abc, 0xd18cf7, 0x26a5d3));
        assert_eq!(
            colors(&Vim9RenderStyle::default(), &normal),
            (0x123456, 0x789abc, 0x123456)
        );
    }
}
