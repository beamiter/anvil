//! Buffer text is shaped by Pango and painted by GTK from Vim's semantic RGB
//! render frames. No terminal cells, ANSI escapes, or VTE palette entries are
//! consulted here. VTE remains the PTY/input adapter and compatibility view.

use super::gtk;
use gtk::{gdk, glib, pango, subclass::prelude::*};
use jterm_core::vim9::{
    Vim9BufferPosition, Vim9Command, Vim9InputSnapshot, Vim9PixelRect, Vim9PixelScroll,
    Vim9PointerPhase, Vim9RenderFrame, Vim9RenderMetrics, Vim9RenderRect, Vim9RenderStyle,
    Vim9ScrollAccumulator, Vim9ScrollAnimation, Vim9Session, Vim9Viewport,
};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
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

struct PresentedFrame {
    frame: Arc<Vim9RenderFrame>,
    metrics: Metrics,
    offsets: HashMap<i64, f32>,
    precision_windows: Vec<i64>,
    cursor: Option<Vim9PixelRect>,
}

struct SourceGesture {
    gesture: u64,
    request: u64,
    last: Vim9BufferPosition,
    pending: Option<(Vim9BufferPosition, Vim9PointerPhase)>,
    released: bool,
}

struct PaintState {
    frame: Option<Arc<Vim9RenderFrame>>,
    metrics: Metrics,
    font: pango::FontDescription,
    font_generation: u64,
    ligatures: bool,
    focused: bool,
    preedit: Option<Preedit>,
    time: f64,
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
            time: 0.0,
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

/// Each retained scene supplies rows exposed at the edge while a new Vim
/// viewport eases into place. Keeping the outgoing text prevents blank bands.
#[derive(Clone)]
struct ScrollScene {
    topline: u64,
    node: gtk::gsk::RenderNode,
}

#[derive(Default)]
struct WindowMotion {
    animation: Vim9ScrollAnimation,
    history: Vec<ScrollScene>,
}

#[derive(Default)]
struct ScrollInput {
    fractions: Vim9ScrollAccumulator,
    pending: HashMap<i64, i32>,
    requested: HashMap<i64, f64>,
}

fn paint_shifted_scene(
    snapshot: &gtk::Snapshot,
    node: &gtk::gsk::RenderNode,
    view: &gtk::graphene::Rect,
    offset: f32,
) {
    snapshot.save();
    snapshot.translate(&gtk::graphene::Point::new(0.0, offset));
    snapshot.push_clip(view);
    snapshot.append_node(node);
    snapshot.pop();
    snapshot.restore();
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct BufferSurface {
        pub(super) state: RefCell<PaintState>,
        pub(super) layouts: RefCell<HashMap<LayoutKey, pango::Layout>>,
        pub(super) text_scene: RefCell<Option<TextScene>>,
        pub(super) motions: RefCell<HashMap<i64, WindowMotion>>,
        pub(super) scroll_input: RefCell<ScrollInput>,
        pub(super) pointer: Cell<Option<(f64, f64)>>,
        pub(super) pixels: RefCell<HashMap<i64, Vim9PixelScroll>>,
        pub(super) pixel_enabled: Cell<bool>,
        pub(super) source_gesture: RefCell<Option<SourceGesture>>,
        pub(super) gesture_sequence: Cell<u64>,
        pub(super) presented: RefCell<Option<PresentedFrame>>,
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
                self.presented.borrow_mut().take();
                return;
            };
            let metrics = state.metrics;
            if !metrics.is_valid() {
                self.presented.borrow_mut().take();
                return;
            }
            if !self.pixels.borrow().is_empty() {
                paint_pixel_scene(
                    &surface,
                    snapshot,
                    &state,
                    frame,
                    &mut self.layouts.borrow_mut(),
                );
            } else {
                let mut scene = self.text_scene.borrow_mut();
                if !scene
                    .as_ref()
                    .is_some_and(|cached| cached.matches(&state, frame, surface.scale_factor()))
                {
                    #[cfg(test)]
                    self.scene_builds.set(self.scene_builds.get() + 1);
                    let mut layouts = self.layouts.borrow_mut();
                    *scene = build_text_scene(&surface, &state, frame, &mut layouts).map(|node| {
                        TextScene {
                            frame: frame.clone(),
                            metrics,
                            font_generation: state.font_generation,
                            ligatures: state.ligatures,
                            scale_factor: surface.scale_factor(),
                            node,
                        }
                    });
                }
                if let Some(scene) = scene.as_mut() {
                    // Advance the frame identity after reusing the same scene, so
                    // another focus/preedit snapshot is an O(1) pointer check.
                    scene.frame = frame.clone();
                    let motions = self.motions.borrow();
                    if motions.is_empty() {
                        snapshot.append_node(&scene.node);
                    } else {
                        for window in &frame.windows {
                            let view = view_rectangle(&window.view, metrics);
                            snapshot.push_clip(&view);
                            let (_, background, _) =
                                colors(&frame.default_style, &frame.default_style);
                            snapshot.append_color(&rgb(background), &view);
                            if let Some(motion) = motions.get(&window.window) {
                                let offset = motion.animation.offset(state.time) as f32;
                                if let Some(outgoing) = motion.history.first() {
                                    let delta = window.topline as f64 - outgoing.topline as f64;
                                    paint_shifted_scene(
                                        snapshot,
                                        &outgoing.node,
                                        &view,
                                        offset - (delta * f64::from(metrics.line_height)) as f32,
                                    );
                                }
                                paint_shifted_scene(snapshot, &scene.node, &view, offset);
                            } else {
                                snapshot.append_node(&scene.node);
                            }
                            snapshot.pop();
                        }
                    }
                }
                drop(scene);
            }
            let cursor = surface.projected_cursor();
            *self.presented.borrow_mut() = Some(PresentedFrame {
                frame: frame.clone(),
                metrics,
                offsets: frame
                    .windows
                    .iter()
                    .map(|w| {
                        (
                            w.window,
                            surface.presentation_offset(&state, frame, w.window),
                        )
                    })
                    .collect(),
                precision_windows: self.pixels.borrow().keys().copied().collect(),
                cursor,
            });
            // Cursor and composition are transient layers over the retained
            // buffer scene, and never invalidate unchanged shaped text.
            for region in frame.paint_views() {
                let view = view_rectangle(region, metrics);
                snapshot.push_clip(&view);
                if frame.cursor.visible && region.contains(frame.cursor.row, frame.cursor.column) {
                    let Some(cursor) = cursor.map(graphene_rect) else {
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

fn paint_pixel_scene(
    surface: &BufferSurface,
    snapshot: &gtk::Snapshot,
    state: &PaintState,
    frame: &Vim9RenderFrame,
    layouts: &mut HashMap<LayoutKey, pango::Layout>,
) {
    let metrics = state.metrics;
    let baseline = row_baseline(surface, state);
    if layouts.len() > 4096 {
        layouts.clear();
    }
    for window in &frame.windows {
        let offset = surface.presentation_offset(state, frame, window.window);
        let Some(view) = Vim9Viewport::new(frame, window.window, metrics, offset) else {
            continue;
        };
        snapshot.push_clip(&graphene_rect(view.clip()));
        let (_, background, _) = colors(&frame.default_style, &frame.default_style);
        snapshot.append_color(&rgb(background), &graphene_rect(view.clip()));
        for projected in view.runs() {
            let (_, background, _) = colors(&projected.run.style, &frame.default_style);
            snapshot.append_color(&rgb(background), &graphene_rect(projected.clip));
        }
        for projected in view.runs() {
            let run = projected.run;
            let x = projected.rect.x;
            let y = projected.rect.y;
            let width = projected.rect.width;
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
                    snapshot,
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
        surface.pango_context().set_round_glyph_positions(false);
        surface.connect_unmap(|surface| {
            surface.cancel_scroll();
            surface.imp().pointer.set(None);
            surface.imp().presented.borrow_mut().take();
        });
        surface.connect_scale_factor_notify(|surface| {
            // Cached layouts belong to the widget's Pango context. Retire
            // them when it moves between monitors with different DPI.
            surface.cancel_scroll();
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
        self.cancel_scroll();
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
        if preedit.is_some() {
            self.cancel_scroll();
        }
        self.imp().state.borrow_mut().preedit = preedit;
        self.queue_draw();
    }

    pub(super) fn is_preediting(&self) -> bool {
        self.imp().state.borrow().preedit.is_some()
    }

    pub(super) fn cancel_scroll(&self) {
        self.imp().pixels.borrow_mut().clear();
        self.imp().source_gesture.borrow_mut().take();
        self.imp().motions.borrow_mut().clear();
        let mut input = self.imp().scroll_input.borrow_mut();
        input.fractions.clear();
        input.pending.clear();
        input.requested.clear();
        self.queue_draw();
    }

    /// Called before replacing the acknowledged frame. It never animates a
    /// guessed viewport or a changed buffer, split layout or font geometry.
    fn retarget_scroll(
        &self,
        next: &Option<Arc<Vim9RenderFrame>>,
        metrics: Metrics,
        now: f64,
        enabled: bool,
    ) {
        let state = self.imp().state.borrow();
        let mut motions = self.imp().motions.borrow_mut();
        if !self.imp().pixels.borrow().is_empty() {
            motions.clear();
            return;
        }
        let was_animating = !motions.is_empty();
        if was_animating {
            self.queue_draw();
        }
        motions.retain(|_, motion| motion.animation.is_active(now));
        let (Some(previous), Some(next)) = (&state.frame, next) else {
            motions.clear();
            return;
        };
        if !enabled
            || state.metrics != metrics
            || previous.default_style != next.default_style
            || previous.windows.len() != next.windows.len()
            || next.windows.is_empty()
        {
            motions.clear();
            return;
        }
        // Unchanged frames take an O(1) path even on high-refresh monitors.
        if Arc::ptr_eq(previous, next) {
            return;
        }
        let input = self.imp().scroll_input.borrow();
        let mut scene = self.imp().text_scene.borrow_mut();
        if !scene
            .as_ref()
            .is_some_and(|scene| scene.matches(&state, previous, self.scale_factor()))
        {
            let requested_movement = next.windows.iter().any(|window| {
                input
                    .requested
                    .get(&window.window)
                    .is_some_and(|time| now - time < 0.25)
                    && previous
                        .windows
                        .iter()
                        .any(|old| old.window == window.window && old.topline != window.topline)
            });
            if requested_movement {
                // Precision snapshots paint directly and do not update the
                // retained integer scene. Rebuild it lazily when a real wheel
                // acknowledgement needs that outgoing viewport; otherwise the
                // first wheel after a touchpad gesture would snap instead of
                // animating. Cached Pango layouts remain reusable.
                #[cfg(test)]
                self.imp()
                    .scene_builds
                    .set(self.imp().scene_builds.get() + 1);
                *scene =
                    build_text_scene(self, &state, previous, &mut self.imp().layouts.borrow_mut())
                        .map(|node| TextScene {
                            frame: previous.clone(),
                            metrics,
                            font_generation: state.font_generation,
                            ligatures: state.ligatures,
                            scale_factor: self.scale_factor(),
                            node,
                        });
            }
        }
        let Some(scene) = scene
            .as_ref()
            .filter(|scene| scene.matches(&state, previous, self.scale_factor()))
        else {
            motions.clear();
            return;
        };
        for window in &next.windows {
            let old = previous
                .windows
                .iter()
                .find(|old| old.window == window.window);
            let Some(old) = old.filter(|old| {
                old.buffer == window.buffer
                    && old.changedtick == window.changedtick
                    && old.view == window.view
                    && old.leftcol == window.leftcol
                    && old.textoff == window.textoff
            }) else {
                motions.remove(&window.window);
                continue;
            };
            if old.topline == window.topline {
                continue;
            }
            let delta =
                (window.topline as f64 - old.topline as f64) * f64::from(metrics.line_height);
            let recent = input
                .requested
                .get(&window.window)
                .is_some_and(|time| now - time < 0.25);
            if !recent || delta.abs() >= window.view.rows as f64 * f64::from(metrics.line_height) {
                motions.remove(&window.window);
                continue;
            }
            let motion = motions.entry(window.window).or_default();
            motion.animation.retarget(delta, now);
            motion.history.push(ScrollScene {
                topline: old.topline,
                node: scene.node.clone(),
            });
            // Paint the newest outgoing viewport that covers the exposed
            // edge. Keep its newer candidates too: a sustained gesture will
            // need them once this source moves beyond the visible span.
            // Older scenes outside the overlap are discarded every frame.
            let offset = motion.animation.offset(now);
            let height = window.view.rows as f64 * f64::from(metrics.line_height);
            let outgoing = motion.history.iter().rposition(|scene| {
                let distance =
                    (window.topline as f64 - scene.topline as f64) * f64::from(metrics.line_height);
                if offset >= 0.0 {
                    distance >= offset && distance <= height
                } else {
                    distance <= offset && distance >= -height
                }
            });
            if let Some(outgoing) = outgoing.filter(|_| offset.abs() < height) {
                motion.history.drain(..outgoing);
            } else {
                motions.remove(&window.window);
            }
        }
        if was_animating || !motions.is_empty() {
            self.queue_draw();
        }
    }

    pub(super) fn enqueue_scroll(&self, x: f64, y: f64, dy: f64, unit: gdk::ScrollUnit) -> bool {
        if self.stale_scroll_source_at(x, y) {
            // The event belongs to the image still on screen. Do not send it
            // to a replacement buffer before GTK has presented that buffer.
            self.cancel_scroll();
            return true;
        }
        let state = self.imp().state.borrow();
        let Some(frame) = &state.frame else {
            return false;
        };
        let Some((row, column)) = state.metrics.cell_at(x as f32, y as f32) else {
            return false;
        };
        let Some(window) = frame
            .windows
            .iter()
            .find(|window| window.view.contains(row, column))
        else {
            return false;
        };
        if !dy.is_finite() || state.preedit.is_some() {
            return false;
        }
        let lines = match unit {
            gdk::ScrollUnit::Surface => dy / f64::from(state.metrics.line_height),
            _ => dy * 3.0,
        };
        let mut input = self.imp().scroll_input.borrow_mut();
        if self.imp().source_gesture.borrow().is_some() {
            return true;
        }
        if unit == gdk::ScrollUnit::Surface
            && self.imp().pixel_enabled.get()
            && frame.mode == "n"
            && window.native_pointer
            && !input.pending.contains_key(&window.window)
        {
            let mut pixels = self.imp().pixels.borrow_mut();
            if let Some(scroll) = pixels.get_mut(&window.window) {
                scroll.push(lines);
            } else if let Some(mut scroll) = Vim9PixelScroll::new(window, state.metrics) {
                scroll.push(lines);
                pixels.insert(window.window, scroll);
            } else {
                return false;
            }
            input.fractions.discard(window.window);
            self.imp().motions.borrow_mut().clear();
            self.queue_draw();
            return true;
        }
        // Discrete wheels keep their established acknowledged-row animation.
        self.imp().pixels.borrow_mut().clear();
        let count = input.fractions.add(window.window, lines);
        if count != 0 {
            let pending = input.pending.entry(window.window).or_default();
            *pending = (*pending + count).clamp(-120, 120);
        }
        true
    }

    fn presentation_offset(&self, state: &PaintState, frame: &Vim9RenderFrame, window: i64) -> f32 {
        if let Some(scroll) = self.imp().pixels.borrow().get(&window) {
            return scroll.offset(frame, state.metrics);
        }
        self.imp()
            .motions
            .borrow()
            .get(&window)
            .map_or(0.0, |motion| motion.animation.offset(state.time) as f32)
    }

    fn projected_cursor(&self) -> Option<Vim9PixelRect> {
        let state = self.imp().state.borrow();
        let frame = state.frame.as_ref()?;
        let offset = self.presentation_offset(&state, frame, frame.window);
        if self.imp().pixels.borrow().contains_key(&frame.window) {
            return Vim9Viewport::new(frame, frame.window, state.metrics, offset)?.cursor_rect();
        }
        let mut cursor = state.metrics.cursor_rect(&frame.cursor)?;
        cursor.y += offset;
        Some(cursor)
    }

    /// The IM candidate rectangle must follow the caret already submitted to
    /// GTK, even when the polling loop has received a newer cursor/scroll frame.
    /// Geometry and buffer switches retire it until a replacement is painted.
    fn presented_cursor(&self) -> Option<Vim9PixelRect> {
        let state = self.imp().state.borrow();
        let current = state.frame.as_ref()?;
        let presented = self.imp().presented.borrow();
        let presented = presented.as_ref()?;
        if state.metrics != presented.metrics
            || current.window != presented.frame.window
            || current.buffer != presented.frame.buffer
        {
            return None;
        }
        presented.cursor
    }

    /// Hit the last submitted native image, not a newer frame received between
    /// presentation ticks. Source generation is checked again before dispatch.
    fn source_at(&self, x: f64, y: f64) -> Option<Vim9BufferPosition> {
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        let state = self.imp().state.borrow();
        if state.preedit.is_some() {
            return None;
        }
        let current = state.frame.as_ref()?;
        if !matches!(current.mode.as_str(), "n" | "v") {
            return None;
        }
        let presented = self.imp().presented.borrow();
        let presented = presented.as_ref()?;
        if presented.metrics != state.metrics {
            return None;
        }
        for window in &presented.frame.windows {
            let live = current.windows.iter().find(|w| w.window == window.window)?;
            if !window.native_pointer
                || !live.native_pointer
                || live.buffer != window.buffer
                || live.changedtick != window.changedtick
                || live.view != window.view
                || live.leftcol != window.leftcol
                || live.textoff != window.textoff
            {
                continue;
            }
            let offset = presented
                .offsets
                .get(&window.window)
                .copied()
                .unwrap_or(0.0);
            if let Some(view) =
                Vim9Viewport::new(&presented.frame, window.window, presented.metrics, offset)
            {
                if let Some(anchor) = view.anchor_at(x as f32, y as f32) {
                    return Some(anchor.position);
                }
            }
        }
        None
    }

    fn pointer_command(
        position: Vim9BufferPosition,
        gesture: u64,
        phase: Vim9PointerPhase,
    ) -> Vim9Command {
        Vim9Command::Pointer {
            window: position.window,
            buffer: position.buffer,
            changedtick: position.changedtick,
            line: position.line,
            byte_column: position.byte_column,
            gesture,
            phase,
        }
    }

    fn precision_image_at(&self, x: f64, y: f64) -> bool {
        if !x.is_finite() || !y.is_finite() {
            return false;
        }
        self.imp().presented.borrow().as_ref().is_some_and(|shown| {
            shown.frame.windows.iter().any(|window| {
                shown.precision_windows.contains(&window.window)
                    && shown
                        .offsets
                        .get(&window.window)
                        .is_some_and(|offset| *offset != 0.0)
                    && shown
                        .metrics
                        .rect(&window.view)
                        .is_some_and(|rect| rect.contains(x as f32, y as f32))
            })
        })
    }

    fn stale_precision_at(&self, x: f64, y: f64) -> bool {
        self.precision_image_at(x, y) && self.source_at(x, y).is_none()
    }

    fn should_settle_press(&self, x: f64, y: f64, button: u32, count: i32, modified: bool) -> bool {
        self.stale_precision_at(x, y)
            || (self.precision_image_at(x, y) && (button != 1 || count != 1 || modified))
    }

    fn stale_scroll_source_at(&self, x: f64, y: f64) -> bool {
        if !x.is_finite() || !y.is_finite() {
            return false;
        }
        let presented = self.imp().presented.borrow();
        let Some(shown) = presented.as_ref() else {
            return false;
        };
        let Some(window) = shown.frame.windows.iter().find(|window| {
            shown
                .metrics
                .rect(&window.view)
                .is_some_and(|rect| rect.contains(x as f32, y as f32))
        }) else {
            return false;
        };
        let state = self.imp().state.borrow();
        state.metrics != shown.metrics
            || state.frame.as_ref().is_none_or(|current| {
                current
                    .windows
                    .iter()
                    .find(|live| live.window == window.window)
                    .is_none_or(|live| {
                        live.buffer != window.buffer
                            || live.changedtick != window.changedtick
                            || live.view != window.view
                            || live.leftcol != window.leftcol
                            || live.textoff != window.textoff
                    })
            })
    }

    fn direct_source_at(&self, x: f64, y: f64) -> Option<Vim9BufferPosition> {
        let position = self.source_at(x, y)?;
        // Do not replace stationary Vim mouse behavior. Source transactions
        // are necessary only while precision presentation differs from its
        // integer grid; plain clicks/multi-clicks otherwise remain Vim-owned.
        let presented = self.imp().presented.borrow();
        let presented = presented.as_ref()?;
        // Cancellation/acknowledgement may retire the live pixel controller
        // before GTK paints its replacement. Ownership follows the submitted
        // image until that image is replaced, just like its hit coordinates.
        if !presented.precision_windows.contains(&position.window) {
            return None;
        }
        let offset = presented.offsets.get(&position.window).copied()?;
        (offset != 0.0).then_some(position)
    }

    fn press_source(&self, session: &Vim9Session, x: f64, y: f64) -> bool {
        let Some(position) = self.direct_source_at(x, y) else {
            return false;
        };
        let gesture = self.imp().gesture_sequence.get() % i32::MAX as u64 + 1;
        let Ok(request) = session.send(Self::pointer_command(
            position,
            gesture,
            Vim9PointerPhase::Press,
        )) else {
            // This press targets a visible fractional image. Failed transport
            // must settle it, not fall through to different terminal cells.
            self.cancel_scroll();
            return true;
        };
        self.imp().gesture_sequence.set(gesture);
        *self.imp().source_gesture.borrow_mut() = Some(SourceGesture {
            gesture,
            request,
            last: position,
            pending: None,
            released: false,
        });
        // Drop unsent legacy wheel impulses; an already accepted source-bound
        // scroll stays ordered ahead of the click on the same bridge channel.
        let mut input = self.imp().scroll_input.borrow_mut();
        input.pending.clear();
        input.fractions.clear();
        input.requested.clear();
        self.imp().motions.borrow_mut().clear();
        true
    }

    fn queue_source_pointer(&self, x: f64, y: f64, phase: Vim9PointerPhase) -> bool {
        let target = self.source_at(x, y);
        let mut gesture = self.imp().source_gesture.borrow_mut();
        let Some(gesture) = gesture.as_mut() else {
            return false;
        };
        if gesture.released {
            return true;
        }
        let position = target.filter(|p| {
            p.window == gesture.last.window
                && p.buffer == gesture.last.buffer
                && p.changedtick == gesture.last.changedtick
        });
        if phase == Vim9PointerPhase::Release {
            // Releasing outside the split completes at the last admitted
            // source target, never at another split or fabricated source row.
            gesture.pending = Some((position.unwrap_or(gesture.last), phase));
            gesture.released = true;
        } else if let Some(position) = position {
            if position != gesture.last {
                gesture.pending = Some((position, phase));
                gesture.last = position;
            }
        }
        true
    }

    fn source_motion(&self, x: f64, y: f64, primary_down: bool) {
        if primary_down {
            self.queue_source_pointer(x, y, Vim9PointerPhase::Drag);
        } else if let Some(gesture) = self.imp().source_gesture.borrow_mut().as_mut() {
            // Some compositors/widget grabs lose the release event. A later
            // hover must finish at the last admitted target, never extend the
            // selection to the unpressed pointer's new location.
            if !gesture.released {
                gesture.pending = Some((gesture.last, Vim9PointerPhase::Release));
                gesture.released = true;
            }
        }
    }

    fn advance_native_input(
        &self,
        session: &Vim9Session,
        input: &Vim9InputSnapshot,
        next: &Option<Arc<Vim9RenderFrame>>,
        metrics: Metrics,
        enabled: bool,
    ) {
        self.imp().pixel_enabled.set(enabled);
        let Some(frame) = next.as_ref() else {
            self.imp().pixels.borrow_mut().clear();
            self.imp().source_gesture.borrow_mut().take();
            // Keep last-image ownership until GTK actually clears/replaces
            // the surface; an intervening click must not hit a different grid.
            return;
        };
        let changed_geometry = {
            let state = self.imp().state.borrow();
            state.metrics != metrics
                || state.frame.as_ref().is_some_and(|old| {
                    old.rows != frame.rows
                        || old.columns != frame.columns
                        || old.windows.len() != frame.windows.len()
                        || old
                            .windows
                            .iter()
                            .zip(&frame.windows)
                            .any(|(a, b)| a.window != b.window || a.view != b.view)
                })
        };
        let mut pointer = self.imp().source_gesture.borrow_mut();
        if !enabled
            || changed_geometry
            || !matches!(frame.mode.as_str(), "n" | "v")
            || pointer.as_ref().is_some_and(|gesture| {
                !frame.windows.iter().any(|w| {
                    w.window == gesture.last.window
                        && w.buffer == gesture.last.buffer
                        && w.changedtick == gesture.last.changedtick
                        && w.native_pointer
                })
            })
        {
            *pointer = None;
        }
        let mut pixels = self.imp().pixels.borrow_mut();
        let before = pixels.len();
        if !enabled || changed_geometry || (frame.mode != "n" && pointer.is_none()) {
            pixels.clear();
        } else {
            pixels.retain(|window, scroll| {
                frame
                    .windows
                    .iter()
                    .find(|w| w.window == *window)
                    .is_some_and(|w| {
                        let pending = scroll
                            .request_id()
                            .is_some_and(|id| input.command_pending(id));
                        scroll.reconcile(w, metrics, pending)
                    })
            });
        }
        if pixels.len() != before {
            self.queue_draw();
        }
        if pointer.is_none() && frame.mode == "n" {
            for scroll in pixels.values_mut() {
                if let Some(command) = scroll.command() {
                    if let Ok(id) = session.send(command) {
                        scroll.submitted(id);
                    }
                }
            }
        }
        if let Some(gesture) = pointer.as_mut() {
            if !input.command_pending(gesture.request) {
                if let Some((position, phase)) = gesture.pending.take() {
                    match session.send(Self::pointer_command(position, gesture.gesture, phase)) {
                        Ok(id) => {
                            gesture.request = id;
                            gesture.last = position;
                        }
                        // Fail closed on transport saturation/disconnect. Do
                        // not replay an old drag after unrelated keyboard input.
                        Err(_) => {
                            *pointer = None;
                        }
                    }
                } else if gesture.released {
                    *pointer = None;
                }
            }
        }
    }

    #[cfg(test)]
    pub(super) fn scroll_offset(&self, window: i64) -> f64 {
        let time = self.imp().state.borrow().time;
        self.imp()
            .motions
            .borrow()
            .get(&window)
            .map_or(0.0, |motion| motion.animation.offset(time))
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
        smooth_enabled: bool,
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
        let smooth = gtk::CheckButton::with_label("Smooth scrolling");
        smooth.set_active(smooth_enabled);
        smooth.set_tooltip_text(Some(
            "Direct precision-device pixels with source-checked clicks; discrete wheels use bounded easing. Obeys reduced-motion settings.",
        ));
        settings.append(&smooth);
        {
            let surface = surface.clone();
            smooth.connect_toggled(move |_| surface.cancel_scroll());
        }
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

        let motion = gtk::EventControllerMotion::new();
        motion.set_name(Some("vim9-buffer-pointer"));
        motion.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let surface = surface.downgrade();
            motion.connect_motion(move |controller, x, y| {
                if let Some(surface) = surface.upgrade() {
                    surface.imp().pointer.set(Some((x, y)));
                    surface.source_motion(
                        x,
                        y,
                        controller
                            .current_event_state()
                            .contains(gdk::ModifierType::BUTTON1_MASK),
                    );
                }
            });
        }
        {
            let surface = surface.downgrade();
            motion.connect_leave(move |_| {
                if let Some(surface) = surface.upgrade() {
                    surface.imp().pointer.set(None);
                }
            });
        }
        terminal.add_controller(motion);
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
        scroll.set_name(Some("vim9-buffer-smooth-scroll"));
        scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let surface = surface.downgrade();
            scroll.connect_scroll(move |controller, dx, dy| {
                let Some(surface) = surface.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                let Some((x, y)) = surface.imp().pointer.get() else {
                    return glib::Propagation::Proceed;
                };
                if surface.stale_scroll_source_at(x, y) {
                    surface.cancel_scroll();
                    return glib::Propagation::Stop;
                }
                if controller.current_event_state().intersects(
                    gdk::ModifierType::CONTROL_MASK
                        | gdk::ModifierType::ALT_MASK
                        | gdk::ModifierType::SUPER_MASK
                        | gdk::ModifierType::SHIFT_MASK,
                ) || dy == 0.0
                    || dx.abs() > dy.abs()
                {
                    if surface.precision_image_at(x, y) {
                        surface.cancel_scroll();
                        return glib::Propagation::Stop;
                    }
                    return glib::Propagation::Proceed;
                }
                if surface.enqueue_scroll(x, y, dy, controller.unit()) {
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            });
        }
        terminal.add_controller(scroll);
        // Keyboard/preedit cancels unsent native gestures before PTY input.
        let editing = gtk::EventControllerKey::new();
        editing.set_name(Some("vim9-buffer-scroll-stop"));
        editing.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let surface = surface.downgrade();
            editing.connect_key_pressed(move |_, _, _, _| {
                if let Some(surface) = surface.upgrade() {
                    surface.cancel_scroll();
                }
                glib::Propagation::Proceed
            });
        }
        terminal.add_controller(editing);
        let clicking = gtk::GestureClick::new();
        clicking.set_button(0);
        clicking.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let surface = surface.downgrade();
            let terminal = terminal.downgrade();
            let session = session.clone();
            clicking.connect_pressed(move |gesture, count, x, y| {
                let Some(surface) = surface.upgrade() else {
                    return;
                };
                let modified = gesture.current_event_state().intersects(
                    gdk::ModifierType::CONTROL_MASK
                        | gdk::ModifierType::SHIFT_MASK
                        | gdk::ModifierType::ALT_MASK
                        | gdk::ModifierType::SUPER_MASK,
                );
                if surface.should_settle_press(x, y, gesture.current_button(), count, modified) {
                    // Consume unsupported clicks and stale images until the
                    // fractional view settles. Their integer terminal cells
                    // would not address the source currently on screen.
                    surface.cancel_scroll();
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    return;
                }
                if gesture.current_button() == 1
                    && count == 1
                    && !modified
                    && surface.press_source(&session, x, y)
                {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    if let Some(terminal) = terminal.upgrade() {
                        terminal.grab_focus();
                    }
                } else {
                    surface.cancel_scroll();
                }
            });
        }
        {
            let surface = surface.downgrade();
            clicking.connect_released(move |gesture, _, x, y| {
                if gesture.current_button() != 1 {
                    return;
                }
                if let Some(surface) = surface.upgrade() {
                    if surface.queue_source_pointer(x, y, Vim9PointerPhase::Release) {
                        gesture.set_state(gtk::EventSequenceState::Claimed);
                    }
                }
            });
        }
        {
            let surface = surface.downgrade();
            clicking.connect_unpaired_release(move |_, x, y, button, _| {
                if button == 1 {
                    if let Some(surface) = surface.upgrade() {
                        surface.queue_source_pointer(x, y, Vim9PointerPhase::Release);
                    }
                }
            });
        }
        {
            let surface = surface.downgrade();
            clicking.connect_cancel(move |_, _| {
                if let Some(surface) = surface.upgrade() {
                    surface.cancel_scroll();
                }
            });
        }
        terminal.add_controller(clicking);

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
                if !terminal.has_focus() {
                    surface.cancel_scroll();
                }
                surface.imp().state.borrow_mut().focused = terminal.has_focus();
                surface.queue_draw();
            });
        }
        let session = session.clone();
        let weak_terminal = terminal.downgrade();
        let poll_surface = surface.clone();
        let poll_badge = badge.clone();
        let poll = RefCell::new((None, 0, 0, String::new()));
        // Follow the compositor's frame clock (including high refresh displays)
        // rather than a fixed 16 ms polling timer. Hidden widgets do no work.
        widget.add_tick_callback(move |_, clock| {
            let mut poll = poll.borrow_mut();
            let (subscribed, font_generation, render_revision_floor, old_status) = &mut *poll;
            let now = clock.frame_time() as f64 / 1_000_000.0;
            let Some(terminal) = weak_terminal.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let wanted = enabled.is_active();
            let input = session.input_snapshot();
            let frame = input.frame.clone();
            let generation = poll_surface.imp().state.borrow().font_generation;
            if generation != *font_generation {
                *font_generation = generation;
                *render_revision_floor = frame.as_ref().map_or(0, |frame| frame.revision);
                // A font-family/style change can retain the same PTY geometry.
                // Re-subscribing explicitly requests a fresh keyframe in that
                // case, instead of waiting forever for a Vim resize event.
                *subscribed = None;
            }
            // send() is bounded and refuses unauthenticated sessions; retry
            // subscription after startup rather than silently losing it.
            if *subscribed != Some(wanted)
                && session
                    .send(Vim9Command::SetRender { enabled: wanted })
                    .is_ok()
            {
                *subscribed = Some(wanted);
            }
            let geometry_matches = frame.as_ref().is_some_and(|frame| {
                frame.revision > *render_revision_floor
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
            let animations =
                smooth.is_active() && poll_surface.settings().is_gtk_enable_animations();
            poll_surface.advance_native_input(
                &session,
                &input,
                &next,
                metrics,
                active && animations,
            );
            poll_surface.retarget_scroll(&next, metrics, now, animations);
            {
                let mut input = poll_surface.imp().scroll_input.borrow_mut();
                input.requested.retain(|_, time| now - *time < 0.25);
                if active {
                    let pending = std::mem::take(&mut input.pending);
                    for (window, lines) in pending {
                        if lines == 0
                            || !frame.as_ref().is_some_and(|frame| {
                                frame.windows.iter().any(|view| view.window == window)
                            })
                        {
                            continue;
                        }
                        if session.send(Vim9Command::Scroll { window, lines }).is_ok() {
                            input.requested.insert(window, now);
                        } else {
                            input.pending.insert(window, lines);
                        }
                    }
                } else {
                    input.pending.clear();
                    input.fractions.clear();
                    input.requested.clear();
                }
            }
            let mut state = poll_surface.imp().state.borrow_mut();
            let changed = match (&state.frame, &next) {
                (Some(previous), Some(next)) => !Arc::ptr_eq(previous, next),
                (None, None) => false,
                _ => true,
            } || state.metrics != metrics;
            state.metrics = metrics;
            state.time = now;
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
                if let Some(cursor) = poll_surface.presented_cursor() {
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
            if status != *old_status {
                poll_badge.set_label(&status);
                poll_badge.set_tooltip_text(Some(&status));
                render_status.set_label(&status);
                *old_status = status;
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

    #[test]
    #[ignore = "requires GTK display; exercised by the native pixel integration CI"]
    fn native_pixel_snapshot_and_source_hit_round_trip() {
        gtk::init().expect("GTK display required");
        use jterm_core::vim9::{
            Vim9RenderCursor, Vim9RenderMarginRow, Vim9RenderRun, Vim9RenderWindow,
        };
        let surface = BufferSurface::new();
        let metrics = Metrics {
            origin_x: 0.0,
            origin_y: 0.0,
            cell_width: 8.0,
            line_height: 20.0,
        };
        let cursor = Vim9RenderCursor {
            row: 1,
            column: 0,
            width: 1,
            visible: true,
            line: 11,
            byte_column: 0,
        };
        let region = Vim9RenderRect {
            row: 0,
            column: 0,
            rows: 4,
            columns: 8,
        };
        let run = |row, line, text: &str| Vim9RenderRun {
            window: 10,
            row,
            column: 0,
            cells: text.len(),
            text: text.into(),
            line,
            byte_start: 0,
            byte_end: text.len(),
            ..Default::default()
        };
        let frame = Arc::new(Vim9RenderFrame {
            revision: 1,
            supported: true,
            rows: 6,
            columns: 8,
            window: 10,
            buffer: 1,
            changedtick: 3,
            mode: "n".into(),
            view: region.clone(),
            cursor: cursor.clone(),
            windows: vec![Vim9RenderWindow {
                window: 10,
                buffer: 1,
                changedtick: 3,
                view: region,
                cursor,
                topline: 10,
                botline: 13,
                line_count: 100,
                native_pointer: true,
                line_lengths: vec![2, 2, 2, 0],
                overscan: vec![
                    Vim9RenderMarginRow {
                        row: -1,
                        line: 9,
                        byte_length: 3,
                        runs: vec![run(0, 9, "pre")],
                    },
                    Vim9RenderMarginRow {
                        row: 4,
                        line: 14,
                        byte_length: 4,
                        runs: vec![run(0, 14, "post")],
                    },
                ],
                ..Default::default()
            }],
            runs: vec![run(0, 10, "AB"), run(1, 11, "CD"), run(2, 12, "EF")],
            ..Default::default()
        });
        {
            let mut state = surface.imp().state.borrow_mut();
            state.metrics = metrics;
            state.frame = Some(frame.clone());
        }
        surface.imp().pixel_enabled.set(true);
        assert!(surface.enqueue_scroll(8.0, 10.0, 6.25, gdk::ScrollUnit::Surface));
        assert_eq!(
            surface.presentation_offset(&surface.imp().state.borrow(), &frame, 10),
            -6.25
        );
        assert_eq!(surface.projected_cursor().unwrap().y, 13.75);
        assert!(surface.presented_cursor().is_none());
        // Execute the actual GTK snapshot/Pango path, including a partly
        // exposed semantic margin. No GPU/hardware latency is asserted here.
        let snapshot = gtk::Snapshot::new();
        surface.imp().snapshot(&snapshot);
        assert!(snapshot.to_node().is_some());
        assert_eq!(surface.presented_cursor().unwrap().y, 13.75);
        assert_eq!(surface.source_at(9.0, 75.0).unwrap().line, 14);
        assert_eq!(surface.source_at(9.0, 75.0).unwrap().byte_column, 1);
        assert_eq!(
            surface.direct_source_at(9.0, 75.0),
            surface.source_at(9.0, 75.0)
        );
        assert_eq!(surface.source_at(45.0, 65.0).unwrap().line, 13);
        assert!(surface.source_at(65.0, 75.0).is_none());
        assert!(!surface.should_settle_press(9.0, 75.0, 1, 1, false));
        for (button, count, modified) in [(3, 1, false), (1, 2, false), (1, 1, true)] {
            assert!(surface.should_settle_press(9.0, 75.0, button, count, modified));
        }
        // Input must still hit the submitted image when a new frame is
        // received before GTK has submitted its replacement snapshot.
        let mut received = (*frame).clone();
        received.revision += 1;
        received.windows[0].topline += 1;
        received.windows[0].botline += 1;
        received.cursor.row += 1;
        received.windows[0].cursor.row += 1;
        surface.imp().state.borrow_mut().frame = Some(Arc::new(received));
        assert_eq!(surface.source_at(9.0, 75.0).unwrap().line, 14);
        assert_eq!(surface.presented_cursor().unwrap().y, 13.75);
        assert_ne!(surface.projected_cursor(), surface.presented_cursor());
        assert!(
            !surface.stale_scroll_source_at(9.0, 75.0),
            "a valid viewport acknowledgement may change topline before paint"
        );
        surface.imp().state.borrow_mut().frame = Some(frame.clone());

        // A lost release must not turn later hover movement into a drag. End
        // at the last accepted source position even when hovering elsewhere.
        let last = surface.source_at(9.0, 75.0).unwrap();
        *surface.imp().source_gesture.borrow_mut() = Some(SourceGesture {
            gesture: 1,
            request: 1,
            last,
            pending: None,
            released: false,
        });
        surface.source_motion(9.0, 10.0, false);
        {
            let gesture = surface.imp().source_gesture.borrow();
            let gesture = gesture.as_ref().unwrap();
            assert!(gesture.released);
            assert_eq!(gesture.last, last);
            assert_eq!(gesture.pending, Some((last, Vim9PointerPhase::Release)));
        }
        surface.source_motion(9.0, 30.0, true);
        assert_eq!(
            surface.imp().source_gesture.borrow().as_ref().unwrap().last,
            last
        );
        surface.cancel_scroll();
        assert!(surface.imp().pixels.borrow().is_empty());
        assert!(surface.imp().source_gesture.borrow().is_none());
        assert_eq!(surface.presented_cursor().unwrap().y, 13.75);
        assert_eq!(
            surface.direct_source_at(9.0, 75.0),
            Some(last),
            "retiring input state must not remap an image GTK still displays"
        );
        let snapshot = gtk::Snapshot::new();
        surface.imp().snapshot(&snapshot);
        assert!(snapshot.to_node().is_some());
        assert_eq!(surface.source_at(9.0, 75.0).unwrap().line, 13);
        assert_eq!(surface.presented_cursor().unwrap().y, 20.0);
        assert!(
            surface.direct_source_at(9.0, 75.0).is_none(),
            "stationary input remains Vim-owned"
        );
        assert!(!surface.stale_precision_at(9.0, 75.0));
        for (button, count, modified) in [(3, 1, false), (1, 2, false), (1, 1, true)] {
            assert!(!surface.should_settle_press(9.0, 75.0, button, count, modified));
        }
        assert!(surface.enqueue_scroll(8.0, 10.0, 6.25, gdk::ScrollUnit::Surface));
        let snapshot = gtk::Snapshot::new();
        surface.imp().snapshot(&snapshot);
        assert!(snapshot.to_node().is_some());
        let mut edited = (*frame).clone();
        edited.windows[0].changedtick += 1;
        surface.imp().state.borrow_mut().frame = Some(Arc::new(edited));
        assert!(surface.source_at(9.0, 75.0).is_none());
        assert!(surface.stale_precision_at(9.0, 75.0));
        assert!(surface.should_settle_press(9.0, 75.0, 1, 1, false));
        for unit in [gdk::ScrollUnit::Surface, gdk::ScrollUnit::Wheel] {
            assert!(surface.enqueue_scroll(9.0, 75.0, 1.0, unit));
            assert!(surface.imp().pixels.borrow().is_empty());
            assert!(surface.imp().scroll_input.borrow().pending.is_empty());
        }
        let mut replaced = (*frame).clone();
        replaced.windows[0].buffer += 1;
        surface.imp().state.borrow_mut().frame = Some(Arc::new(replaced));
        assert!(surface.stale_scroll_source_at(9.0, 75.0));
        let mut resized = (*frame).clone();
        resized.windows[0].view.columns -= 1;
        surface.imp().state.borrow_mut().frame = Some(Arc::new(resized));
        assert!(surface.stale_scroll_source_at(9.0, 75.0));
        assert!(
            surface.stale_precision_at(9.0, 75.0),
            "cancel has not replaced the visible image"
        );
        surface.imp().state.borrow_mut().frame = None;
        assert!(
            surface.stale_precision_at(9.0, 75.0),
            "an inactive pending frame still has an old visible image"
        );
        let snapshot = gtk::Snapshot::new();
        surface.imp().snapshot(&snapshot);
        assert!(snapshot.to_node().is_none());
        assert!(!surface.stale_precision_at(9.0, 75.0));
    }
}
