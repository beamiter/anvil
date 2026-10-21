//! Buffer text is shaped by Pango and painted by GTK from Vim's semantic RGB
//! render frames. No terminal cells, ANSI escapes, or VTE palette entries are
//! consulted here. VTE remains the PTY/input adapter and compatibility view.

use super::gtk;
use gtk::{gdk, glib, pango, subclass::prelude::*};
use jterm_core::vim9::{
    Vim9Command, Vim9PixelRect, Vim9RenderFrame, Vim9RenderMetrics, Vim9RenderRect,
    Vim9RenderStyle, Vim9Session,
};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use vte4::prelude::*;

const DEFAULT_FOREGROUND: u32 = 0xd9e0ee;
const DEFAULT_BACKGROUND: u32 = 0x121720;

type Metrics = Vim9RenderMetrics;

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
    font_generation: u64,
    ligatures: bool,
    focused: bool,
    preedit: Option<Preedit>,
}

impl Default for PaintState {
    fn default() -> Self {
        Self {
            frame: None,
            metrics: Metrics {
                origin_x: 0.0,
                origin_y: 0.0,
                cell_width: 0.0,
                line_height: 0.0,
            },
            font: pango::FontDescription::from_string("Monospace 12"),
            font_generation: 0,
            ligatures: true,
            focused: false,
            preedit: None,
        }
    }
}

type LayoutKey = (String, bool, bool);

/// Retain the immutable text/background scene independently of cursor, focus
/// and input-method overlays. A cursor-only frame reuses GTK render nodes.
struct TextScene {
    frame: Arc<Vim9RenderFrame>,
    metrics: Metrics,
    font_generation: u64,
    ligatures: bool,
    scale_factor: i32,
    node: gtk::gsk::RenderNode,
}

fn same_text_content(previous: &Vim9RenderFrame, next: &Vim9RenderFrame) -> bool {
    previous.default_style == next.default_style
        && previous.paint_views().eq(next.paint_views())
        && previous.runs.len() == next.runs.len()
        && (previous.runs == next.runs
            // Full frames arrive grouped by window; row deltas are grouped by
            // screen row. Compare the actual per-view painting order so that
            // their equivalent interleaving does not rebuild the text scene.
            || previous.paint_views().all(|view| {
                previous
                    .runs
                    .iter()
                    .filter(|run| view.contains(run.row, run.column))
                    .eq(next
                        .runs
                        .iter()
                        .filter(|run| view.contains(run.row, run.column)))
            }))
}

impl TextScene {
    fn matches(&self, state: &PaintState, frame: &Arc<Vim9RenderFrame>, scale_factor: i32) -> bool {
        self.metrics == state.metrics
            && self.font_generation == state.font_generation
            && self.ligatures == state.ligatures
            && self.scale_factor == scale_factor
            && (Arc::ptr_eq(&self.frame, frame) || same_text_content(&self.frame, frame))
    }
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct BufferSurface {
        pub(super) state: RefCell<PaintState>,
        pub(super) layouts: RefCell<HashMap<LayoutKey, pango::Layout>>,
        pub(super) text_scene: RefCell<Option<TextScene>>,
        #[cfg(test)]
        pub(super) painted_revision: Cell<u64>,
        #[cfg(test)]
        pub(super) scene_builds: Cell<usize>,
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
            if !metrics.is_valid() {
                return;
            }
            let mut scene = self.text_scene.borrow_mut();
            if !scene
                .as_ref()
                .is_some_and(|cached| cached.matches(&state, frame, surface.scale_factor()))
            {
                #[cfg(test)]
                self.scene_builds.set(self.scene_builds.get() + 1);
                let mut layouts = self.layouts.borrow_mut();
                *scene =
                    build_text_scene(&surface, &state, frame, &mut layouts).map(|node| TextScene {
                        frame: frame.clone(),
                        metrics,
                        font_generation: state.font_generation,
                        ligatures: state.ligatures,
                        scale_factor: surface.scale_factor(),
                        node,
                    });
            }
            if let Some(scene) = scene.as_mut() {
                // Advance the frame identity after reusing the same scene, so
                // another focus/preedit snapshot is an O(1) pointer check.
                scene.frame = frame.clone();
                snapshot.append_node(&scene.node);
            }
            drop(scene);
            // Cursor and composition are transient layers over the retained
            // buffer scene, and never invalidate unchanged shaped text.
            for region in frame.paint_views() {
                let view = view_rectangle(region, metrics);
                snapshot.push_clip(&view);
                if frame.cursor.visible && region.contains(frame.cursor.row, frame.cursor.column) {
                    let Some(cursor) = metrics.cursor_rect(&frame.cursor).map(graphene_rect) else {
                        snapshot.pop();
                        continue;
                    };
                    let (foreground, _, _) = colors(&frame.default_style, &frame.default_style);
                    paint_cursor(snapshot, &cursor, foreground, &frame.mode, state.focused);
                    if let Some(preedit) = state.preedit.as_ref() {
                        paint_preedit(&surface, snapshot, &state, preedit, &cursor, &view);
                    }
                }
                snapshot.pop();
            }
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

fn build_text_scene(
    surface: &BufferSurface,
    state: &PaintState,
    frame: &Vim9RenderFrame,
    layouts: &mut HashMap<LayoutKey, pango::Layout>,
) -> Option<gtk::gsk::RenderNode> {
    let snapshot = gtk::Snapshot::new();
    let metrics = state.metrics;
    // Clip and cover each Vim editing window independently. A single
    // bounding rectangle would erase live PTY status lines, split
    // separators and plugin-owned chrome between the native panes.
    let baseline = row_baseline(surface, state);
    if layouts.len() > 4096 {
        layouts.clear();
    }
    for region in frame.paint_views() {
        let view = view_rectangle(region, metrics);
        snapshot.push_clip(&view);
        let (_, background, _) = colors(&frame.default_style, &frame.default_style);
        snapshot.append_color(&rgb(background), &view);

        // Paint backgrounds before ink so a subsequent syntax run
        // cannot erase italic glyphs from the previous run.
        for run in frame
            .runs
            .iter()
            .filter(|run| region.contains(run.row, run.column))
        {
            let (_, background, _) = colors(&run.style, &frame.default_style);
            snapshot.append_color(
                &rgb(background),
                &gtk::graphene::Rect::new(
                    metrics.origin_x + run.column as f32 * metrics.cell_width,
                    metrics.origin_y + run.row as f32 * metrics.line_height,
                    run.cells as f32 * metrics.cell_width,
                    metrics.line_height,
                ),
            );
        }

        // Layouts are cached across cursor and viewport updates. Pango
        // shapes a complete same-style run, including font fallback and
        // supported ligatures; GTK retains device-independent text nodes.
        for run in frame
            .runs
            .iter()
            .filter(|run| region.contains(run.row, run.column))
        {
            let x = metrics.origin_x + run.column as f32 * metrics.cell_width;
            let y = metrics.origin_y + run.row as f32 * metrics.line_height;
            let width = run.cells as f32 * metrics.cell_width;
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
                // A fallback CJK font can have different ascent and
                // descent from the Latin font. Centering every run's
                // logical box independently makes their baselines drift.
                let text_y = y + baseline - layout.baseline() as f32 / pango::SCALE as f32;
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
                decorations(
                    &snapshot,
                    &run.style,
                    special,
                    x,
                    y,
                    width,
                    metrics.line_height,
                );
            }
        }

        snapshot.pop();
    }
    snapshot.to_node()
}

fn graphene_rect(rect: Vim9PixelRect) -> gtk::graphene::Rect {
    gtk::graphene::Rect::new(rect.x, rect.y, rect.width, rect.height)
}

fn view_rectangle(view: &Vim9RenderRect, metrics: Metrics) -> gtk::graphene::Rect {
    metrics
        .rect(view)
        .map(graphene_rect)
        .unwrap_or_else(|| gtk::graphene::Rect::new(0.0, 0.0, 0.0, 0.0))
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

fn row_baseline(surface: &BufferSurface, state: &PaintState) -> f32 {
    let metrics = surface.pango_context().metrics(Some(&state.font), None);
    let ascent = metrics.ascent() as f32 / pango::SCALE as f32;
    let descent = metrics.descent() as f32 / pango::SCALE as f32;
    (state.metrics.line_height - ascent - descent) * 0.5 + ascent
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
    let (width, _) = layout.pixel_size();
    let x = cursor
        .x()
        .min((view.x() + view.width() - width as f32 - 4.0).max(view.x()));
    let y =
        cursor.y() + row_baseline(surface, state) - layout.baseline() as f32 / pango::SCALE as f32;
    let (foreground, background, _) = colors(&frame.default_style, &frame.default_style);
    snapshot.append_color(
        &rgb(background),
        &gtk::graphene::Rect::new(x, cursor.y(), width as f32 + 4.0, state.metrics.line_height),
    );
    snapshot.save();
    snapshot.translate(&gtk::graphene::Point::new(x, y));
    snapshot.append_layout(&layout, &rgb(foreground));
    snapshot.restore();
    snapshot.append_color(
        &rgb(foreground),
        &gtk::graphene::Rect::new(
            x,
            cursor.y() + state.metrics.line_height - 1.0,
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
        &gtk::graphene::Rect::new(caret_x, cursor.y(), 1.0, state.metrics.line_height),
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
        surface.connect_scale_factor_notify(|surface| {
            // Cached layouts belong to the widget's Pango context. Retire
            // them when it moves between monitors with different DPI.
            surface.imp().layouts.borrow_mut().clear();
            surface.imp().text_scene.borrow_mut().take();
            surface.queue_draw();
        });
        surface.update_property(&[gtk::accessible::Property::Label(
            "Native Vim buffer rendering",
        )]);
        surface
    }

    fn set_font(&self, font: &pango::FontDescription) {
        let mut state = self.imp().state.borrow_mut();
        state.font = font.clone();
        state.font_generation = state.font_generation.wrapping_add(1);
        // Font metrics and VTE's PTY allocation change asynchronously. Do not
        // paint the old geometry with the new font while a resized frame is
        // in flight; the live Vim surface owns this short transition.
        state.frame = None;
        drop(state);
        self.imp().layouts.borrow_mut().clear();
        self.imp().text_scene.borrow_mut().take();
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
    pub(super) fn scene_build_count(&self) -> usize {
        self.imp().scene_builds.get()
    }

    #[cfg(test)]
    pub(super) fn painted_frame(&self) -> Option<Arc<Vim9RenderFrame>> {
        let frame = self.imp().state.borrow().frame.clone()?;
        (self.imp().painted_revision.get() == frame.revision).then_some(frame)
    }
}

fn zoom_font(font_button: &gtk::FontDialogButton, step: Option<f64>) {
    let Some(mut font) = font_button.font_desc() else {
        return;
    };
    let current = f64::from(font.size()) / f64::from(pango::SCALE);
    let size = step.map_or(12.0, |step| (current + step).clamp(6.0, 72.0));
    if font.is_size_absolute() {
        font.set_absolute_size(size * f64::from(pango::SCALE));
    } else {
        font.set_size((size * f64::from(pango::SCALE)).round() as i32);
    }
    font_button.set_font_desc(&font);
}

pub(super) struct NativeBuffer {
    pub widget: gtk::Overlay,
    pub menu: gtk::MenuButton,
    pub badge: gtk::Label,
    pub surface: BufferSurface,
}

impl NativeBuffer {
    pub(super) fn new(
        session: &Vim9Session,
        terminal: &vte4::Terminal,
        native_enabled: bool,
    ) -> Self {
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
        let render_status = gtk::Label::new(Some("GUI · connecting"));
        render_status.set_xalign(0.0);
        render_status.set_wrap(true);
        render_status.add_css_class("caption");
        settings.append(&render_status);
        let enabled = gtk::CheckButton::with_label("Native GUI buffer");
        enabled.set_active(native_enabled);
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

        // Zoom belongs to the buffer even when the workbench chrome is off.
        // Updating the font control uses the same frame invalidation and real
        // Vim-grid resize path as the native font chooser.
        let zoom_keys = gtk::EventControllerKey::new();
        zoom_keys.set_name(Some("vim9-buffer-zoom"));
        zoom_keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let font = font.downgrade();
            let surface = surface.downgrade();
            zoom_keys.connect_key_pressed(move |_, key, _, modifiers| {
                if !modifiers.contains(gdk::ModifierType::CONTROL_MASK)
                    || modifiers
                        .intersects(gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK)
                    || surface
                        .upgrade()
                        .is_some_and(|surface| surface.is_preediting())
                {
                    return glib::Propagation::Proceed;
                }
                let step = match key {
                    gdk::Key::plus | gdk::Key::equal | gdk::Key::KP_Add => Some(Some(1.0)),
                    gdk::Key::minus | gdk::Key::KP_Subtract => Some(Some(-1.0)),
                    gdk::Key::_0 | gdk::Key::KP_0 => Some(None),
                    _ => None,
                };
                match (font.upgrade(), step) {
                    (Some(font), Some(step)) => {
                        zoom_font(&font, step);
                        glib::Propagation::Stop
                    }
                    _ => glib::Propagation::Proceed,
                }
            });
        }
        terminal.add_controller(zoom_keys);
        let zoom_scroll = gtk::EventControllerScroll::new(
            gtk::EventControllerScrollFlags::VERTICAL | gtk::EventControllerScrollFlags::DISCRETE,
        );
        zoom_scroll.set_name(Some("vim9-buffer-zoom-scroll"));
        zoom_scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let font = font.downgrade();
            let surface = surface.downgrade();
            zoom_scroll.connect_scroll(move |controller, _, dy| {
                if !controller
                    .current_event_state()
                    .contains(gdk::ModifierType::CONTROL_MASK)
                    || controller
                        .current_event_state()
                        .intersects(gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK)
                    || surface
                        .upgrade()
                        .is_some_and(|surface| surface.is_preediting())
                    || !dy.is_finite()
                    || dy == 0.0
                {
                    return glib::Propagation::Proceed;
                }
                let Some(font) = font.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                zoom_font(&font, Some(-dy.signum()));
                glib::Propagation::Stop
            });
        }
        terminal.add_controller(zoom_scroll);

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
        let mut font_generation = 0;
        let mut render_revision_floor = 0;
        let mut old_status = String::new();
        glib::timeout_add_local(Duration::from_millis(16), move || {
            if weak_widget.upgrade().is_none() {
                return glib::ControlFlow::Break;
            }
            let Some(terminal) = weak_terminal.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let wanted = enabled.is_active();
            let frame = session.render_snapshot();
            let generation = poll_surface.imp().state.borrow().font_generation;
            if generation != font_generation {
                font_generation = generation;
                render_revision_floor = frame.as_ref().map_or(0, |frame| frame.revision);
                // A font-family/style change can retain the same PTY geometry.
                // Re-subscribing explicitly requests a fresh keyframe in that
                // case, instead of waiting forever for a Vim resize event.
                subscribed = None;
            }
            // send() is bounded and refuses unauthenticated sessions; retry
            // subscription after startup rather than silently losing it.
            if subscribed != Some(wanted)
                && session
                    .send(Vim9Command::SetRender { enabled: wanted })
                    .is_ok()
            {
                subscribed = Some(wanted);
            }
            let geometry_matches = frame.as_ref().is_some_and(|frame| {
                frame.revision > render_revision_floor
                    && terminal_grid_is_allocated(&terminal)
                    && frame.rows == terminal.row_count().max(0) as usize
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
                if let Some(cursor) = frame
                    .as_ref()
                    .and_then(|frame| metrics.cursor_rect(&frame.cursor))
                {
                    im.set_cursor_location(&gdk::Rectangle::new(
                        cursor.x.round() as i32,
                        cursor.y.round() as i32,
                        cursor.width.ceil() as i32,
                        cursor.height.ceil() as i32,
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
                render_status.set_label(&status);
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
        origin_x: x,
        origin_y: y,
        cell_width: terminal.char_width().max(1) as f32,
        line_height: terminal.char_height().max(1) as f32,
    }
}

fn terminal_grid_is_allocated(terminal: &vte4::Terminal) -> bool {
    let border = gtk::prelude::ScrollableExt::border(terminal);
    let (horizontal, vertical) = border.as_ref().map_or((0, 0), |border| {
        (
            i32::from(border.left()) + i32::from(border.right()),
            i32::from(border.top()) + i32::from(border.bottom()),
        )
    });
    let columns = (terminal.width() - horizontal).max(0) as i64 / terminal.char_width().max(1);
    let rows = (terminal.height() - vertical).max(0) as i64 / terminal.char_height().max(1);
    // set_font changes character metrics before GTK reallocates VTE's PTY.
    // Geometry is ready only when the published counts fit the current size.
    rows > 0 && columns > 0 && rows == terminal.row_count() && columns == terminal.column_count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_text_scene_ignores_cursor_but_invalidates_real_paint_changes() {
        let frame = Vim9RenderFrame {
            view: Vim9RenderRect {
                row: 0,
                column: 0,
                rows: 4,
                columns: 20,
            },
            runs: vec![jterm_core::vim9::Vim9RenderRun {
                row: 0,
                column: 0,
                text: "text".into(),
                cells: 4,
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut moved = frame.clone();
        moved.revision += 1;
        moved.cursor.column = 2;
        moved.mode = "i".into();
        assert!(same_text_content(&frame, &moved));
        moved.runs[0].style.foreground = Some(0x123456);
        assert!(!same_text_content(&frame, &moved));
        moved = frame.clone();
        moved.view.column += 1;
        assert!(!same_text_content(&frame, &moved));
        moved = frame.clone();
        moved.default_style.background = Some(0x102030);
        assert!(!same_text_content(&frame, &moved));
    }

    #[test]
    fn retained_text_scene_accepts_equivalent_full_frame_and_row_delta_order() {
        use jterm_core::vim9::{Vim9RenderRun, Vim9RenderWindow};
        let mut frame = Vim9RenderFrame::default();
        for column in [0, 21] {
            frame.windows.push(Vim9RenderWindow {
                view: Vim9RenderRect {
                    row: 0,
                    column,
                    rows: 2,
                    columns: 20,
                },
                ..Default::default()
            });
            for row in 0..2 {
                frame.runs.push(Vim9RenderRun {
                    row,
                    column,
                    text: format!("{row}, {column}"),
                    cells: 5,
                    ..Default::default()
                });
            }
        }
        let mut delta = frame.clone();
        delta.runs.sort_by_key(|run| run.row);
        assert_ne!(frame.runs, delta.runs);
        assert!(same_text_content(&frame, &delta));
        delta.runs[0].style.background = Some(0x123456);
        assert!(!same_text_content(&frame, &delta));
    }

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
